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

## Own-enumerable semantics for object rest and spread (P7, 2026-11-09)

Object-rest (`{...r} = src`) and object spread (`{...src}`) copied the raw
`props` entries: an accessor was copied as an `Acc` value instead of being
invoked, and non-enumerable own props were copied too. Both paths now walk
`for_in_keys` (own, enumerable, hidden keys filtered) and read each value
through `get_member`, so a getter runs exactly once and a prop created with
`Object.defineProperty(..., { enumerable: false })` is skipped. `define_prop`
now honors an explicit `enumerable: false` by recording the key as hidden
(`obj_hide`/`clo_hide`) instead of ignoring the flag.

### Measured

- test262 language subset: 13,827 -> **13,960** pass (79.0% of 17,678 run,
  6,154 skipped).
- Every `*-obj-ptrn-rest-getter` and `*-obj-ptrn-rest-skip-non-enumerable`
  case now passes (0 failures, was 56 each).
- engine tests **939** green (new object-rest/spread test).

## Lazy array destructuring with IteratorClose (P7, 2026-11-09)

`bind_pat` for an array pattern called `iter_src`, which drained the whole
iterator. That over-consumed side-effecting generators (an elision such as
`[,]` ran the body twice) and never closed a partially-used iterator, so
`{ next(), return() }` objects never saw `return`. The binder now uses
`iter_limited`: it pulls exactly `parts.len()` values (all of them when a
rest element is present), tracks whether the iterator signalled `done`, and
calls the iterator's `return` hook when it stopped early. The array-pattern
parser was also off by one: `[,]` parsed as two holes and `[a,]` as a hole
after the binding, because it treated a trailing comma as an elision. It now
consumes a comma at an element position as one hole, matching the array
literal parser, so `[a,]`, `[,]` and `[a,,b]` count correctly.

### Measured

- test262 language subset: 13,960 -> **14,261** pass (80.7% of 17,678 run,
  6,154 skipped).
- `ary-ptrn-elision`, `ary-ptrn-empty`, `ary-init-iter-close`,
  `ary-ptrn-elem-ary-elision-init` and `ary-ptrn-elem-ary-empty-init` all
  drop to **0 failures** (was 56 each).
- engine tests **940** green (new lazy/close test).

## Function `name`/`length` own-property descriptors (P7, 2026-11-09)

`Object.getOwnPropertyDescriptor(fn, "name")` returned `undefined`: the
synthesized `name`/`length` slots were readable through member access but
invisible to descriptors, so test262's `verifyProperty` died reading
`.value` of `undefined`. `fn_desc` now materializes both slots with the
spec attributes (non-writable, non-enumerable, configurable) and reports a
real function's `prototype` as writable/non-enumerable/non-configurable.
`set_member` treats a write to an undeleted `name`/`length` as a silent
sloppy no-op, and `delete` records the deletion in a small `ClosureMeta`
bitmask so later reads, `hasOwnProperty`, `in` and descriptors all agree
the slot is gone.

### Measured

- test262 language subset: 14,261 -> **14,320** pass (81.0% of 17,678 run,
  6,154 skipped).
- `fn-name` failures 77 -> **36**; `init-fn-name` failures 30 -> **10**.
- engine tests **941** green (new descriptor test).

## Immutable named-function-expression self-binding (P7, 2026-11-09)

A named function expression binds its own name inside its body, but that
binding is immutable: in sloppy mode `function g() { g = 1 }` must leave `g`
as the function. The engine declared the self-binding like an ordinary
variable, so the assignment replaced it. `Env` now has a third binding kind
(`read_only`) whose `set_own` reports the write as handled but drops the new
value; `declare_read_only` installs it for the function-expression
self-name. Function *declarations* keep a mutable binding.

### Measured

- test262 language subset: 14,320 -> **14,328** pass (81.0% of 17,678 run,
  6,154 skipped).
- `named-no-strict-reassign-fn-name-in-body*` failures 6 -> **0**.
- engine tests **942** green (new immutable-binding test).

## Function `length` stops at the first default/rest parameter (P7, 2026-11-09)

`fn.length` and its descriptor reported `params.len()`, so
`function (a, b = 1, c) {}` claimed 3 and trailing-comma/default forms were
off. `FnDef::length` now counts formal parameters up to the first one with a
default initializer, never counting a rest parameter (`(a, ...r)` -> 1,
`(...r)` -> 0, `(a, b = 1)` -> 1). Both the member read and the descriptor
use it, which also gives setters the right answer (a setter with a default
parameter has length 0).

### Measured

- test262 language subset: 14,328 -> **14,360** pass (81.2% of 17,678 run,
  6,154 skipped).
- Every `length descriptor` / `length is properly set` failure (31) now
  passes.
- engine tests **943** green (new `length` test).

## Regex literals are validated at parse time (P7, 2026-11-09)

A regex literal kept its raw pattern until first evaluation, so an invalid
pattern was neither a parse error nor a runtime error until the literal was
reached. test262's parse-phase negative cases evaluate `$DONOTEVALUATE()`
first and reported "expected SyntaxError, got string". The parser now runs
`regex::compile` over each `Tok::Regex` and turns a `RegexError` into a
SyntaxError with the pattern/flags in the message. Compilation is cached per
(pattern, flags) by the evaluator anyway, so validating early costs one
compile per literal.

### Measured

- test262 language subset: 14,360 -> **14,517** pass (82.1% of 17,678 run,
  6,154 skipped).
- `language/literals/regexp` failures 197 -> **40**; the total failure count
  dropped by exactly the same 157, so no other area regressed.
- engine tests **944** green (new parse-time validation test).

## CRLF line continuations in string literals (P7, 2026-11-09)

The string scanner skipped an escape with a flat `pos += 2`. For a line
continuation written as backslash + CRLF that skipped the backslash and the
CR but left the LF, which the scanner then reported as "unterminated string
literal". The escape path now consumes the whole CRLF pair when present
(and still a single byte otherwise). The unescaper already dropped
backslash + line terminator from the value.

### Measured

- test262 language subset: 14,517 -> **14,527** pass (82.2% of 17,678 run,
  6,154 skipped).
- `unterminated string literal` failures 20 -> **2**; the NIST
  `line-continuation-single`/`-double` cases pass.
- engine tests **945** green (new continuation test).

## CoverInitializedName in object destructuring (P7, 2026-11-09)

Assignment destructuring rejected `{x = default}` with `expected Comma, found
Op(Assign)`: the object-literal parser had no shorthand-initializer form. It
now parses `{x = default}` into a new `ObjElem::CoverInit` variant, and
`expr_to_pat` converts it to `Prop { key, Bind(Name(key), Some(default)) }`,
applying NamedEvaluation so `({fn = function(){}} = {})` still names the
function `fn`.

A cover form is only legal once the literal becomes a pattern, so the parser
tracks `cover_depth`: parentheses, array elements, object property values and
a for-in/of head raise it while their contents parse. After every
`assign_expr` at depth 0, `expr_has_cover_init` rejects a surviving
CoverInit (`var o = {x = 1}`, `f({x = 1})`, `for ({x = 1}; ;)`), while
`{x: y = 1}` stays an ordinary value expression.

### Measured

- test262 language subset: 14,527 -> **14,555** pass (82.3% of 17,678 run,
  6,154 skipped).
- `expected Comma, found Op(Assign)` failures 36 -> **0**; `dstr` failures
  184 -> **156** (28 now pass, the other 8 advance to separate yield/error
  checks). No other directory changed, so the total delta is the same -28.
- engine tests **946** green (new cover-init test).

## `delete` of a `with`-scoped identifier (P7, 2026-11-09)

`delete ident` was a blanket no-op returning false. Inside `with (obj)`,
`delete p` must delete the object's property: the identifier reference
resolves to the object environment record, and the object is the base of the
delete. `JsEngine::env_delete` walks the scope chain in resolution order
(with object, then bindings), deletes an own property from the with object
and reports true, refuses to delete an ordinary binding (false), and reports
true for an unresolvable name. The interpreter's `Delete` arm now routes an
`Ident` through it.

### Measured

- test262 language subset: 14,555 -> **14,598** pass (82.6% of 17,678 run,
  6,154 skipped).
- `statements/with` failures 72 -> **36**; `myObj.p3 === undefined` failures
  30 -> **0**. Seven more `delete`-through-`with` cases pass elsewhere and
  every other top directory is unchanged.
- engine tests **947** green (new with-delete test).

## `with` boxes a primitive head (P7, 2026-11-09)

`with (2)` threw `TypeError: with requires an object`. The head is
`ToObject(expr)`, so a Number/String/Boolean primitive becomes its wrapper
object. The `with` arm now builds a boxed primitive (`ObjKind::Boxed`) with
the matching intrinsic prototype, then uses it as the object environment
record; `null`/`undefined` still throw. A wrapper with no `foo` makes
`with (2) { foo = 42 }` fall through to the enclosing scope, as the spec
requires.

### Measured

- test262 language subset: 14,598 -> **14,601** pass (82.6% of 17,678 run,
  6,154 skipped).
- `with requires an object` failures 3 -> **0** (`12.10-2-1/2/3`).
- engine tests **948** green (new primitive-boxing test).

## Direct eval cannot hoist a var over a parameter binding (P7, 2026-11-10)

EvalDeclarationInstantiation walks from the eval's lexical environment to its
variable environment and throws SyntaxError when a var name is already bound
in between. In practice a sloppy direct eval inside a parameter default may
not declare a name held by the parameter environment (a parameter, or the
implicit `arguments` object), nor a `var` over a block `let`/catch binding on
the way out. `JsEngine::run_program` now performs that collision walk for
direct eval (never for `exec`, which always targets the global environment),
using the parsed body's VarDeclaredNames without descending into nested
function bodies. A call frame records the function whose parameter list it is
binding, so the walk treats the frame as the parameter environment: pending
parameter names plus the implicit `arguments` object, which an arrow does not
have. Strict eval gets a fresh variable environment and is skipped.

### Measured

- test262 language subset: 14,601 -> **14,749** pass (83.4% of 17,678 run,
  6,154 skipped).
- `eval-code/direct` failures 176 -> **39**; `declare-arguments` failures
  141 -> **5** (the five left are arrow-body `arguments` resolution, a
  different gap). No directory regressed.
- engine tests **949** green (new collision test).

## Lexical declarations need a block, and `const` needs an initializer (P7, 2026-11-10)

Two ES2015 early errors the parser was missing. `LexicalDeclaration` (and
`ClassDeclaration`) is not a `Statement`, so a bare `let`/`const`/`class`
cannot be the body of `if`/`else`, `while`, `do`, `for`, `with` or a label;
only a block may contain one. The parser now routes every single-statement
body position through `sub_stmt`, which rejects those three tokens before
parsing. Separately, a `const` declarator requires an initializer
(`const x;`, `const x = 1, y;`, `for (const x;;)`), checked in statement,
classic-`for`-head and `export` positions; `for (const x in/of ...)` binds
through a ForBinding and stays legal.

### Measured

- test262 language subset: 14,749 -> **14,804** pass (83.7% of 17,678 run,
  6,154 skipped).
- "expected SyntaxError, got string" failures 445 -> **374**;
  `statements/const/syntax` and `statements/let/syntax` are clean, with
  `statements/for-of` 137 -> 135 and `statements/for-in` 45 -> 43 as
  side effects. No directory regressed.
- engine tests **951** green (two new early-error tests).

## Arrow parameter early errors (P7, 2026-11-10)

Two ArrowParameters rules. Arrows are stricter than functions: duplicate
bound names across the whole parameter list (including destructuring leaves
and rest) are an early error in every mode, so `(x, x)`, `(x, [x])`,
`({y: x, x})` and `(x, ...x)` are rejected at parse time. The grammar also
carries `ArrowParameters [no LineTerminator here] =>`, so a newline between
the parameters and `=>` ends the expression through ASI instead
(`var af = x\n=> {}` is a SyntaxError). All three arrow detection paths —
single-identifier, `async x =>`, and the parenthesised scan — now honour the
line-terminator restriction.

### Measured

- test262 language subset: 14,804 -> **14,822** pass (83.8% of 17,678 run,
  6,154 skipped).
- "expected SyntaxError, got string" failures 374 -> **356**;
  `expressions/arrow-function` 33 -> **17** (two further fixes elsewhere). No
  directory regressed.
- engine tests **953** green (two new early-error tests).

## Method parameter lists are unique (P7, 2026-11-10)

`MethodDefinition` and accessors use `UniqueFormalParameters`, so a duplicate
bound name is an early error even in sloppy mode, where a plain function
declaration may still repeat a name. The parser's parameter-list builder now
takes a `unique` flag; object-literal methods (plain, computed, `async`,
generator), object accessors and class methods/accessors/constructors request
the unique form. Destructured and rest leaves count, so `m({a}, [a])` is
rejected too.

### Measured

- test262 language subset: 14,822 -> **14,826** pass (83.9% of 17,678 run,
  6,154 skipped).
- "expected SyntaxError, got string" failures 356 -> **352**;
  `expressions/object` 123 -> **121**, `statements/class` 495 -> **493**. No
  directory regressed.
- engine tests **954** green (new method-parameter test).

## Non-ASCII whitespace ends an identifier (P7, 2026-11-10)

The identifier scanner reads raw bytes and accepts any byte `>= 0x80`, which
kept UTF-8 names allocation-free but also swallowed a following non-ASCII
space or line terminator into the name: `x\u00A0+= 1` lexed as the single
identifier `x\u00A0`, so the statement read an undeclared binding. The scan
now decodes the next char at a multibyte position and stops on any
whitespace/control code point (NBSP, the Zs block, U+2028/U+2029), matching
`skip_ws`. Genuine non-ASCII identifiers (`Ä`, Greek letters) are unaffected.

### Measured

- test262 language subset: 14,826 -> **14,867** pass (84.1% of 17,678 run,
  6,154 skipped).
- `expressions/compound-assignment` 146 -> **135** (all 11 `*-whitespace`
  cases), `expressions/assignment` 92 -> **91**; the rest of the gain is
  identifiers followed by a non-ASCII space elsewhere. No directory
  regressed.
- engine tests **955** green (new lexer regression test).

## Computed keys and nested defaults in assignment patterns (P7, 2026-11-10)

A loop head or assignment target destructures through the cover grammar: the
left side is parsed as an object/array literal and then converted by
`expr_to_pat`. That conversion rejected two legal shapes. `{[expr]: target}`
fell into the "computed keys are not supported" arm even though
`ObjPatPart::ComputedProp` and its runtime already existed — it now converts
into that variant. And a nested pattern with a default (`[ {} = 1 ]`,
`{a: [x] = []}`) had already been resolved into a `PatAssign` node by the
assignment parser, which the array/object arms did not recognise; it now
becomes a bound part carrying that default.

### Measured

- test262 language subset: 14,867 -> **14,889** pass (84.2% of 17,678 run,
  6,154 skipped).
- `statements/for-of` 135 -> **124**, `expressions/assignment` 91 -> **80**;
  the 16 `invalid for-of target` failures are gone. No directory regressed.
- engine tests **957** green (two new pattern tests).

## Compound assignment keeps its reference (P7, 2026-11-10)

`lhs op= rhs` must evaluate the reference, `GetValue` it, then evaluate the
right-hand side, then `PutValue` through that same reference. The interpreter
did the opposite: it evaluated the right-hand side first, and it re-resolved
an identifier on the write. Re-resolution broke `with`: the A5 tests use a
getter that deletes its own property, so at write time the object no longer
has the name and the write escaped to the outer `var` instead of creating the
property on the `with` object. Compound and logical assignment now resolve an
identifier's `with` object once, before the read, and write through it (an
ordinary binding or implicit global still goes through `env_set`). The
right-hand side now runs after the target read, as the spec orders it.

### Measured

- test262 language subset: 14,889 -> **14,944** pass (84.5% of 17,678 run,
  6,154 skipped).
- `expressions/compound-assignment` 135 -> **80** (the 32 `with`-scope cases
  and 12 DummyError evaluation-order cases among them). No directory
  regressed.
- engine tests **958** green (new reference-capture test).

## `yield` is an identifier outside generators (P7, 2026-11-10)

The lexer produces a `Yield` keyword token, and the parser treated it as a
yield expression everywhere, so sloppy code such as `var yield = 5`,
`function f(yield) {}` or `{ yield } = o` was rejected with "expected
identifier, found `Kw(Yield)`". Outside a generator, in sloppy code, `yield`
is an ordinary `IdentifierReference`/`BindingIdentifier`: the identifier
sites (`expect_ident`, the binding leaf of a pattern, a formal parameter) now
accept it, and `primary` returns an identifier instead of a yield expression.
Strictness still wins where the parser knows it up front — a class body and
everything nested in it is strict code, so `yield` stays reserved there, and a
class name is a strict `BindingIdentifier` (`class yield {}` is an error). A
function *expression* name has `[~Yield]` even inside a generator body
(`function* g(){ (function yield(){}) })` is legal), while a generator
expression's own name is not (`function* yield(){}` is an early error); the
name is resolved with the nested function's own generator flag. Duplicate
arrow/parameter handling and the strict `"use strict"` prologue are unchanged
(that prologue still does not reserve `yield`, a known gap).

### Measured

- test262 language subset: 14,944 -> **14,994** pass (84.8% of 17,678 run,
  6,154 skipped).
- The 47 `expected identifier, found Kw(Yield)` failures are gone;
  `expressions/assignment` 80 -> **67**, `statements/for-of` 124 -> **111**,
  `expressions/object` 121 -> **114**, `statements/class` 493 -> **492**,
  `expressions/class` 324 -> **323**. No directory regressed.
- engine tests **959** green (new sloppy-`yield` test).

## Object literal `__proto__` sets the prototype (P7, 2026-11-10)

`{__proto__: v}` is not an ordinary property: the colon form with a StringValue
of `__proto__` performs `SetPrototypeOf` on the new object (only when `v` is an
object or null) and creates no own property, and it must not run an inherited
`__proto__` setter. Only the colon form counts — shorthand `{__proto__}`,
computed `{['__proto__']: v}` and a method named `__proto__` are ordinary
properties. Two colon-form entries in one literal are an early error, but only
while the literal stays an expression: in a destructuring pattern the keys are
plain properties (`({__proto__: a, __proto__: b} = o)` is legal), so the check
runs after the cover grammar resolves, reusing the walker that rejects a
surviving `{x = default}`.

Null prototypes had no representation: `Obj.proto == None` also means "the
default prototype attaches lazily on first access", so `Object.create(null)` and
`{__proto__: null}` still reported `Object.prototype`. The flag rides in the
existing `ObjKind::Plain` payload, so `Obj` stays 64 bytes (the allocation test
caught the first attempt, which grew it to 72).

### Measured

- test262 language subset: 14,994 -> **15,000** pass (84.9% of 17,678 run,
  6,154 skipped).
- `expressions/object` 114 -> **108** (all six `__proto__-*` cases including the
  poisoned-`Object.prototype` one). No directory regressed.
- engine tests **960** green (new literal-prototype test, incl. `create(null)`
  and `setPrototypeOf(o, null)`).

## Preload a page's scripts and stylesheets in parallel (P8, 2026-11-10)

A cold github.com load took 28 s here. `MAZDA_TIME` showed where it went:
scripts 6.8 s (fetch 6.4), stylesheet collection ~7 s (not even staged), image
probe 6.1, media probe 2.0 -- and every one of those was a single HTTP/1.1
request at a time. The `[net]` instrumentation added with this change shows why
that matters: on this network a request costs ~300 ms and a fresh TLS
handshake ~700 ms (TCP connect is 8 ms; ping is 81 ms), so 41 sequential
requests are 24.6 s of pure round-trip wait. Chrome hides this with HTTP/2
multiplexing (its github load is 84 resources over `h2`); ureq is HTTP/1.1, so
the equivalent is several connections.

Rule: after parse, fetch the markup's own subresources (`<link rel=stylesheet>`
hrefs and `<script src>`, module entries included, `nomodule` fallbacks skipped
when a module build exists) on a 6-lane pool, then let the existing sequential
phases read them from the disk cache. 6 lanes is the browser budget for HTTP/1.1
per origin; more only queues in the server's accept backlog.

### Measured

- cold github.com: 28,011 -> **21,973 ms** to `[nav] worker finished`.
- `[time] preload: 3991 ms (25 requests, 6 lanes)` replaces ~7 s of sequential
  stylesheet fetches plus most of the script fetches; the script phase drops
  6,835 -> **3,529 ms** (its remaining fetch time is module *imports*, which
  only become known once the entry module parses).
- remaining serial network: image probe 6.4 s, media probe 2.1 s, module
  imports 2.9 s.
- engine tests **960** green (no test change: this is I/O scheduling, not
  language behaviour).

## Probe images on the same lanes (P8b, 2026-11-10)

The image probe walked the box tree and fetched one `<img>` at a time, so it
paid a full round trip per image (11 of them on github.com: 6.4 s). The probe
needs the bytes anyway, and the URLs are already resolved in the box tree's
`ImgSlot::Pending`, so collect them first and run them through the same 6-lane
preload before the sequential walk.

The preload has to send the *same* `Accept` the probe will: the disk cache is
keyed by URL only, so an image preloaded as `text/html` (the document profile)
would poison the entry with the original JPEG and lose the WebP negotiation
that `fetch_image` exists for.

### Measured

- `[time] image probe`: 6,430 -> **3,257–3,746 ms** across three cold runs.
- cold github.com totals: 21,973 -> **17,100 / 18,309 / 18,705 ms** (three
  runs; the page variant changes per request, which moves the CPU phases by
  ~2 s — the phase number is the stable signal).
- engine tests **960** green.

## A bare module specifier is not a URL (P8c, 2026-11-10)

`resolve_specifier` ran every specifier through `Url::join`, so `import * as e
from "react"` became `https://github.githubassets.com/assets/react` -- a real
request that 404s. Per spec a bare specifier is a *package* name: only an
import map can resolve it, and with no import map it is a resolution error
raised before any request. GitHub's bundle has four of these
(`react`, `react-dom`, `react/compiler-runtime`, `react/jsx-runtime`), each
paying a full round trip that always failed.

### Measured

- warm github.com: script fetch 1,740 -> **4 ms**; whole load 4,460 -> **2,746 ms**.
- cold github.com: 17,100–18,700 -> **13,259 ms** to `[nav] worker finished`.
- engine tests **961** green (new `bare_specifiers_do_not_invent_a_url`).
- one 404 remains: `assets/react-dom/client` is imported *by* a module that
  the fix now rejects earlier, so the count is down from five.

## Key the cascade on attributes and `:is()` (P8d, 2026-11-10)

The rightmost-compound index only knew ids, classes and tags, so every rule
whose last compound was `input[type=checkbox]`, `[data-color-mode=dark]` or
`svg:is(...)` sat in the universal bucket and was tested against every element:
2,640 of 15,788 universal entries on GitHub's 31,564 rules, 1.34 M candidate
tests per page. Three keys were missing:

- **attribute**: `input[type=x]` can only match an element carrying `type`.
- **`:is()`/`:where()`**: the element must match ONE alternative, so each
  alternative's own key covers everything the rule matches. If *any* alternate
  yields no key the rule could match an unkeyed element and stays universal.
- **tag fallback**: a compound like `*` plus a pseudo-class has no key at all;
  dropping it from every bucket would lose matches (`MAZDA_TIME` run that did
  exactly that: universal 2,640, 3.04 M candidate tests -- a 2.3x regression).

A rule can now be filed under several buckets (`:is(.a, .b)`), so `candidates`
sorts and dedups before the walk. `[css]` counters (`cascade_stats_take`) print
tested/matched/declarations under `MAZDA_TIME`, which is what showed the
universal split in the first place.

### Measured

- candidates tested: 1,344,659 -> **491,572** (2.7x fewer); universal bucket
  2,640 -> 262 entries, `attrs` 46, `tags` 102.
- `[time] cascade + box tree`, same binary with the old keying behind a
  temporary `MAZDA_CASCADE_OLD` switch, alternating runs on one cached page
  variant: old 3,330/3,532/3,636/3,700 ms vs new
  **2,734/2,798/2,847/3,076 ms** (~0.7 s per load).
- engine tests **961** green (the existing index test now also covers `:is()`
  alternatives and case-insensitive attribute buckets).

## Honour `preload="metadata"` on `<video>` (P9, 2026-11-10)

The media probe fetched every `<video>` body in full just to read its MP4
header. GitHub's landing page ships one hero clip marked `preload="metadata"`
(no `autoplay`): 1.3 MB downloaded on every load, decoded into a first frame
nobody asked to play -- 2-3 s of the cold budget for one header.

A browser reads only enough of the container to size the box, then waits for
the first play to fetch the body. That is exactly a `Range: bytes=0-65535`
request when `moov` precedes `mdat` (faststart, what CDNs ship); the parser
already stops at the first box that overruns the buffer, so a truncated prefix
yields the metadata and nothing else.

- The prefix bytes are kept on disk (`<hash>.head`) because a relayout rebuilds
  the box tree from the DOM and would otherwise re-probe over the network.
- A partial body is never written to the HTTP cache as if it were whole; a
  server that ignores `Range` (`200`) still takes the old whole-body path.
- The slot stays a play target: `MediaEntry` now carries `path: Option` plus
  the source `url`, and the renderer fetches the body on the click that starts
  playback. `shortcut:` that fetch runs on the UI thread, so the first click on
  a deferred video waits for the body (~1 s for the 1.3 MB clip); autoplay
  videos are never deferred.
- With no poster the box gets an opaque black frame (what a browser paints
  before playback), which is also the image slot the renderer swaps for real
  frames.

### Measured

- `[time] media probe`: 1,899-3,777 ms across earlier cold runs -> **332-601 ms**.
- probe traffic: 1,307,532 B -> **65,536 B**; `[net] ... (prefix)`.
- `[time] image decode`: 383-892 ms -> **70-109 ms** (no first-frame decode).
- cold github.com total: ~13.3 s either way (13,259 -> 13,264 ms); the page
  variant moves the other phases by seconds, so the phase numbers above are the
  signal, not the total.
- local range server, real click path: probe 65,536 B / 7 ms, then the first
  click fetched 1,307,532 B in 49 ms and opened the player.
- engine tests **962** green (new
  `preload_metadata_defers_the_body_but_keeps_the_play_target`).

## `<script type="importmap">` (P10, 2026-11-10)

P8c made a bare specifier a resolution error (it must not become
`/assets/react`). That was right per spec but left GitHub's bundle dead: its
first line is `import * as e from "react"`, and the page ships an import map
naming `react`, `react-dom`, `react-dom/client`, `react-is`,
`react-reconciler`, `react/compiler-runtime`, `react/jsx-dev-runtime`,
`react/jsx-runtime` and `scheduler`. With no map, `core-*.js` throws before
any React code runs.

- `collect_import_map` reads the first `<script type="importmap">` (later maps
  are ignored, as the spec says) and `ImportMap::parse` keeps the `imports`
  half, resolving each value against the document URL.
- Keys match exactly, then by the longest key ending in `/` (a prefix mapping,
  `"react/"`). A key whose trailing slash disagrees with its value's is dropped
  (what the spec warns about and skips).
- Only bare specifiers consult the map: relative and absolute specifiers keep
  resolving against the importing module.
- `JSON.parse` and the map share one parser (`builtins::json_parse_value`).
- Dynamic `import()` goes through the same `ModuleLoader::resolve`.

### Measured

- `[js] import map: 9 entries` on github.com (the page's map has 9 imports).
- React now loads and runs: `react-fa5e2e82…js` 10,195 B,
  `react-core-…js` 273,907 B, `react-dom-client-…js` 204,064 B fetched; the
  previous `cannot resolve module specifier` failures are gone.
- 404s on the page: 1 -> **0**.
- `[time] scripts`: 1,635 -> 1,985 ms (fetch 998 -> 1,633 ms): the extra is
  React's own bytes, which used to be skipped entirely.
- cold total 13,264 -> 11,912 ms (page variant noise; not a claim).
- engine tests **965** green (`import_map_resolves_bare_specifiers`,
  `loader_consults_the_map_only_for_bare_specifiers`,
  `import_map_is_collected_and_other_data_blocks_are_not`).
- the first real (non-resolution) errors now surface, both genuine engine
  gaps: `Symbol.for is not a function` in the React bundle, and a regex
  `SyntaxError` for a lookbehind/named group in `react-core`.

## `Symbol.for` / `Symbol.keyFor` (P11, 2026-11-10)

With the import map in place, GitHub's `react-core` bundle died on its second
line, before doing any work:

    var e = Symbol.for("react.transitional.element"), ...

The engine had `Symbol` and its well-known symbols but not the global
registry, so every React element type key vanished. The shim keeps the same
cheap symbol representation (a plain object with a hidden `__symbol`
description), so no `Val`/`Obj` representation changed.

- `Symbol.for(key)` returns one symbol per key; `Symbol.keyFor(sym)` maps back
  to the key and answers `undefined` for a symbol that `for` did not make.
  Both throw `TypeError` on a non-symbol argument.
- The registry is a thread-local table cleared by `JsEngine::new`, next to
  `clear_key_intern`, so a later page does not keep the previous page's
  symbols alive.
- `symbol_desc` is now one helper shared by the prototype methods and the
  statics (the previous inline lookup is gone).

### Measured

- github.com: `Symbol.for is not a function` gone; `react-core` now parses its
  body and reaches the next engine gap instead (the regex named group below).
- engine tests **965** green (assertions added to
  `primitive_and_function_prototypes_exist`: registry identity, `keyFor`,
  stringification of a registered symbol).

## Named capture groups (P12, 2026-11-10)

`react-core` then died on its second regex: `/Minified React error
#(?<invariant>\d+)/`. The parser rejected every `(?<` as "lookbehind / named
groups are not supported", so the whole React core module failed to parse and
the page's interactive bundle never ran.

- `(?<name>...)` is a capture group with the next number, exactly like `(`;
  the name table rides on the compiled `Regex`.
- `\k<name>` compiles to the same `Backref` as `\1`.
- A match result now carries `groups` (one property per name, `undefined` for a
  group that did not participate, and `undefined` -- not `{}` -- when the
  pattern has no names). `exec`, `match`, and `matchAll` all go through the
  same `match_obj`.
- Rejected honestly: duplicate names, empty/invalid names, and an unknown
  `\k<name>`; `new RegExp` keeps those catchable (`try { new
  RegExp("(?<a>x)(?<a>y)") } catch {}`).
- Still gaps, documented: lookbehind (`(?<=`/`(?<!`) has its own error now, and
  `$<name>` in a `replace` template is not substituted yet.

### Measured

- github.com: the `react-core` `SyntaxError` is gone. The page now reaches two
  new gaps instead: "Cannot use `in` operator on function" (from
  `wp-runtime-*.js`) and `\p{L}` unicode property escapes in `wgy-*.js`.
- `[time] scripts` exec on this run: 744 ms (the 2.4 MB landing bundle now
  parses and runs instead of aborting at its first regex).
- engine tests **966** green (new `regex_named_capture_groups`; the unsupported
  syntax test now uses an invalid name rather than a valid named group).

## `in` on built-in functions (P13, 2026-11-10)

The next error from GitHub was from `wp-runtime-*.js`:
`Uncaught TypeError: Cannot use \`in\` operator on function`. The engine only
treated script closures as objects for `in`; every built-in
(`Val::Native`, `Val::Method`) and every callable plain object (`Object`,
`Array`) fell through to the operator's TypeError.

- `Val::Native`: owns `name`/`length`, keeps its statics on the native side
  table, and inherits the rest from `Function.prototype`.
- `Val::Method`: answers the Function.prototype surface only.
- A plain object with the hidden `__call` hook is a function for `in`: it owns
  `name`/`length` and inherits `Function.prototype`.
- Script closures now also see `Function.prototype` (`"call" in f`), not just
  their own `prototype` object.

### Measured

- github.com: the `wp-runtime` `in` TypeError is gone; the next failure from
  that bundle is a separate gap ("undefined is not a function"), and
  `wgy-*.js` still stops at unicode property escapes.
- engine tests **966** green (`in_operator_accepts_function_objects` extended
  with native/method/callable-object cases and the primitive TypeError).

## Unicode property escapes `\p{...}` (P14, 2026-11-10)

`wgy-*.js` (a WordPress bundle loaded by `landing-pages-*.js`) begins its
route matcher with `/[^\p{L}\p{N}\s_-]/gu`. The regex engine rejected every
`\p` as unsupported, so that module never parsed and the page's landing
behaviour stayed dead.

- A property is a class member, so it lives on `ClassSet` as
  `(PropKind, neg)`; it OR's with the ranges and the class-level `^`
  inverts the whole set. `\p{...}`, `\P{...}` and the single-letter `\pL`
  form all parse, and mix with ranges inside one class.
- Answered exactly from `std` char predicates: `L`/`Lu`/`Ll` (alphabetic,
  uppercase, lowercase), `N`/`Nd` (numeric; `Nd` is approximated by
  `is_numeric`), `Z`/`Zs` (whitespace), `ASCII`. Whole-word aliases
  (`Letter`, `Uppercase_Letter`, …) work too.
- Anything else (`\p{So}`, script names, …) stays an honest, catchable
  compile error rather than a silent never-match.

### Measured

- github.com: the `wgy-*.js` `SyntaxError` is gone; `landing-pages-*.js` now
  progresses to the same `wp-runtime` failure the other bundles hit.
- `[time] scripts` exec: 617 -> 723 ms (the `wgy` bundle now parses and runs).
- engine tests **967** green (new `regex_unicode_property_escapes`; the
  unsupported-syntax test now uses `\p{So}`).

## Preload the module graph (P15, 2026-11-10)

On a cold load github.com fetched its chunks one at a time: the loader only
learns a chunk's imports *after* executing it, so ~28 modules paid a
sequential round trip each (~270-530 ms apiece at ~80 ms RTT) after the
initial 6-lane subresource preload had already finished. `[time] scripts`
showed the cost plainly: 12,231 ms, of which 10,867 ms was fetch.

- Before any module body runs, `preload_module_graph` walks the static graph:
  parse each module's `import ... from` / `export ... from` specifiers
  (`js::module::static_deps`), resolve them with the page's import map
  (`resolve_with_map` — the same function the loader uses, so the two cannot
  disagree), and fetch each wave on the same 6 lanes.
- Bounded by 96 modules and 4 waves; dynamic `import()` edges stay lazy, so
  nothing speculative is requested that execution would not have asked for.
- `preload_labeled` reports its own `[time] module preload:` line, because it
  runs inside the script phase and would otherwise hide in `exec`.

### Measured (two cold runs, cache cleared, `MAZDA_TIME=1`)

| phase | before | after |
| --- | --- | --- |
| `[time] scripts` total | 12,231 ms | 5,843 / 4,847 ms |
| of which fetch | 10,867 ms | 314 / 281 ms |
| module preload | - | 34 modules, 3 waves, 3,650 / 3,004 ms |
| `[nav] worker finished` | 24,955 ms | 17,995 / 18,326 ms |

The 3.0-3.7 s of preload wall time is now the floor: 3 MB of chunks over 6
HTTP/1.1 connections. That is where the remaining latency lives (HTTP/2 would
collapse it, but `ureq` 2.12 is 1.1-only).

- engine tests **969** green (new `static_deps_lists_import_and_export_from_sources`,
  `resolve_with_map_serves_the_preloader_the_same_urls`).

## The DOM surface github.com probes at load (P16, 2026-11-10)

With the module graph preloading, github.com's bundles actually run — and each
one died on the first DOM feature the engine did not have. Every fix revealed
the next fatal error, in one greedy sequence:

| after fixing | next fatal error in `core-*.js` |
| --- | --- |
| `Element.hasAttribute` / `matches` / `removeEventListener` | `ReferenceError: Element is not defined` |
| `Element`, `Node` | `ReferenceError: Document is not defined` |
| `Document`, `HTMLDocument` | `ReferenceError: customElements is not defined` |
| `customElements` | `ReferenceError: HTMLButtonElement is not defined` |
| per-tag element interfaces | `TypeError: undefined is not a function` (`e.head.prepend(t)`) |
| `prepend`/`append`/`before`/`after`/`replaceWith`/`replaceChildren` | `TypeError: not an element` (`MutationObserver.observe(document)`) |
| `document` as a node (root id + `nodeType` 9) | `ReferenceError: IntersectionObserver is not defined` |

What this batch adds, and the shape it had to take:

- Element wrappers stay plain objects with their surface as own props. The
  interface globals (`EventTarget`, `Node`, `Element`, `HTMLElement`, 30 tag
  families, `SVGElement`/`SVGSVGElement`) exist to give `instanceof` an honest
  chain: each tag's wrapper is an `HTMLButtonElement`, a `<div>` is not. All of
  them are illegal constructors, like the spec says.
- `customElements.define` records the definition so `get`/`whenDefined` answer
  truthfully, but elements are **not** upgraded: wrappers are built per access,
  so a defined class never runs against an existing element. Logged once per
  name; the page's own `document.createElement` path keeps working.
- `matches`/`closest` reuse the `querySelector` matcher (`tag`, `#id`,
  `.class`) — same documented simple-selector limit.
- `document` carries the arena root id, because `MutationObserver.observe`
  takes a node; its listener bucket is still the window's (it always was, since
  a document without an id had nowhere else to go).

- engine tests **969 → 976** green (new `element_wrappers_are_instances_of_element`,
  `custom_elements_defines_and_answers_get`,
  `dom_insertion_methods_keep_order_and_text`,
  `document_is_a_node_for_observer_and_dispatch`).
- Page render on github.com is unchanged (header only): the bootstrap still
  dies, now on `IntersectionObserver`. The value of this batch is that each
  run gets further into real site code before it stops.

## `IntersectionObserver` that never fires (P17, 2026-11-10)

Next fatal error in the same bootstrap: `ReferenceError: IntersectionObserver
is not defined`. Github's analytics chunk builds one at load
(`new IntersectionObserver(cb, {rootMargin: "0% 0% -30% 0%", threshold: 0})`)
and calls `observe()` on every nav link.

- The interface exists with the honest shape: constructor validating a
  function callback, `root`/`rootMargin`/`thresholds` read back from the
  options, `observe`/`unobserve`/`disconnect`, and `takeRecords()` answering an
  empty list.
- Delivery is the one thing missing, and it is a documented shortcut: an
  intersection needs a viewport and a scroll position, which a page turn does
  not see. "Not intersecting" is exactly an observer that never calls back, so
  lazy-loading pages stop at the first screen instead of lying about what is
  visible. Logged once per page.

### Measured (one cold run, `MAZDA_TIME=1`)

| phase | before | after |
| --- | --- | --- |
| first fatal error in `core-*.js` | `IntersectionObserver is not defined` | `ResizeObserver is not defined` |

- engine tests **976 → 977** green (new
  `intersection_observer_constructs_and_observes_without_firing`).

## Naming the failing site, `ResizeObserver`, and `createElementNS` (P18, 2026-11-10)

Three small steps, each measured by the next fatal error in the same file
(`pb-f417…js`, 3 MB of minified code):

1. `ResizeObserver is not defined`. Same shape as `IntersectionObserver`
   (constructor takes a function, `observe`/`unobserve`/`disconnect`); it also
   never fires, because a box size would have to be read out of the layout tree
   and a page turn does not hold one. Noted once per page.
   Next error: `TypeError: undefined is not a function` — with no location.
2. So a "not a function" error now names its callee offset:
   `TypeError: undefined is not a function (at 223:26892 of https://…/pb-f417…js)`.
   That is the one location this engine can honestly give (no AST is kept for a
   stack trace); the interpreter already stamped the callee offset for its step
   budget, so the cost is only on the error path. Property reads on
   `undefined`/`null` get the statement offset the same way.
3. That location said `document.createElementNS(...)`, which pages use for
   every SVG icon and drag handle. `createElementNS(ns, name)` now creates an
   element from the local name, and SVG tag names map to `SVGElement`
   (a `<path>` is not an `HTMLElement`); the arena has no namespace column, so
   the tag name is what decides the family. SVG *rendering* remains the
   documented gap.

### Measured (one cold run per step, `MAZDA_TIME=1`)

| after | first fatal error |
| --- | --- |
| `ResizeObserver` | `TypeError: undefined is not a function` (no site) |
| call-site location in errors | `Cannot read property 'appendChild' of undefined (at 223:27350 of pb-f417…)` |
| `createElementNS` | `Cannot read property 'appendChild' of undefined` — now inside `f.content.appendChild(p)`, i.e. `<template>.content` |

- engine tests **977 → 980** green (new
  `resize_observer_constructs_and_observes_without_firing`,
  `create_element_ns_makes_svg_elements`, `not_a_function_names_the_call_site`).

## A `var` without an initializer is a no-op (P19, 2026-11-10)

With `<template>.content` in place the bundle got further and died on
`Cannot read property 'initial' of undefined` while reading a stage table
(`class et { constructor() { this.stage = eW.initial } }`). `eW` is a minified
TypeScript enum:

```js
var eM, …, eW, …;                                  // hoisted to the end of the module
(eM = eW || (eW = {}))[eM.initial = 0] = "initial", …;
```

`eW` *was* assigned; then the hoisted `var … eW …;` ran and set it back to
`undefined`. The engine executed every `VarDecl` declarator as an assignment,
so a declaration without an initializer assigned `undefined`.

Per spec a `VariableDeclaration` with no initializer does nothing at runtime:
the binding is created during instantiation (`var`) or evaluation (`let`), and
the statement itself has no effect. That is not a rare shape — minifiers hoist
every `var` to one list, and `function f(x) { var x; }` must keep the
parameter.

- `var` + no initializer: skip. `let x;` still initializes to `undefined`
  (`const x;` stays the documented shortcut).
- engine tests **981 → 982** green (new
  `var_without_initializer_does_not_reset_the_binding`).
- Next fatal error in `core-*.js`:
  `Cannot read property 'requestErrored' of null` — the page calls
  `Object.getPrototypeOf(document.createElement("turbo-frame").delegate)`, i.e.
  it expects `customElements.define` to have upgraded the element.

## A destructured parameter binds in the call frame (P20, 2026-11-10)

Upgrading `turbo-frame` got the bundle past `requestErrored`, and the next
fatal error was `undefined is not a function` at
`core-*.js` 27:19396 — `var rO = n("MC"), rW = n("XE")`, where `n` is the
webpack `require` parameter of the module factory. `n` had worked 7,000
columns earlier in the same factory, so something overwrote it.

In between, the factory runs `let rP = new rT({size: 20})`, and `rT`'s
constructor is

```js
constructor({size: e, min: t, ttl: n, onDelete: r}) { … }
```

A destructured parameter used `BindKind::Decl(VarKind::Var)`, whose name case
called `env_set` — a *scope-chain* write. `n` already existed in the module
env, so `ttl: n` wrote `undefined` there instead of declaring a parameter, and
every later `n(…)` in the factory was a call on `undefined`.

Spec: a parameter is bound in the function's own environment. Now
`BindKind::Param` declares in the call frame (re-binding the same name keeps
the last value, like sloppy duplicate parameters). Plain parameters already
did this; destructured ones did not.

- engine tests **983 → 984** green (new
  `a_destructured_parameter_binds_in_the_call_frame`).
- Next fatal error in `core-*.js`:
  `Cannot read property 'distance' of undefined`.

## Custom elements are upgraded, and their interface constructor yields the element under construction (P21, 2026-11-10)

`document.createElement("turbo-frame").delegate` was `undefined` because
`customElements.define` only recorded the definition. Turbo's frame element is
`class c extends HTMLElement { constructor() { super(); …; this.delegate = new
c.delegateConstructor(this) } }`, so without an upgrade there is no delegate.

Upgrading needs three pieces:

1. **The instance is the wrapper.** `CE_INSTANCES: node → wrapper` caches the
   object the page's constructor ran against; `wrap_element` hands it back
   (ordinary elements still get a fresh wrapper per access). `define` upgrades
   parsed elements, `createElement` and `cloneNode` upgrade new ones.
2. **`super()` works.** The interface native gets the element as `this` (so
   the wrapper's prototype can be the page's class) and returns it; `new
   HTMLElement()` is still `TypeError: Illegal constructor`.
3. **The ES5 adapter works.** GitHub ships
   `custom-elements-es5-adapter.js`: it replaces the global with
   `function () { return Reflect.construct(BuiltInHTMLElement, [],
   this.constructor) }`. Spec: the interface constructor returns the element
   on the definition's construction stack — the element being upgraded. The
   engine keeps that stack in `CE_CONSTRUCTING`; with it empty, construction
   is still illegal.

Before both fixes the cold run died at
`Cannot read property 'requestErrored' of object` (Turbo's frame element
missing `delegate`).

- engine tests **984 → 985** green (through P20; new
  `custom_elements_upgrade_existing_created_and_cloned`,
  `es5_custom_element_adapter_can_construct_on_the_element_under_upgrade`).
- Next fatal error: `Cannot read property 'distance' of undefined`
  (see P20 for the destructured-parameter rule it exposed).

## A property descriptor defaults to non-enumerable (P22, 2026-11-10)

Next fatal error was `Cannot read property 'distance' of undefined` in
`pb-*.js`, inside color-convert's conversion-distance table:

```js
let t = (function () { let e = {}, t = Object.keys(n); … })();
let e = i.pop(), r = Object.keys(n[e]);
let n = r[s], o = t[n];
-1 === o.distance && …
```

`o` was `undefined` for `n === "channels"` (and `"labels"`). color-convert
hides those two keys on every color space:

```js
delete space.channels;
Object.defineProperty(space, "channels", { value: channels });
```

Per spec every descriptor flag defaults to false, so that property must be
non-enumerable. `define_prop` treated a missing `enumerable` as *true* (a
permissive shortcut), so `Object.keys(space)` listed `"channels"` and the
table builder tried to look up a color space called `channels`.

- descriptor flags: `enumerable` absent → false. Only an explicit
  `enumerable: true` shows the property to `for-in` / `Object.keys`.
- engine tests **985 → 986** green (new
  `defined_properties_are_non_enumerable_by_default`).
- Next fatal error: `Uncaught Error: Unable to get document domain`.

## `document.domain` is the page host (P23, 2026-11-10)

GitHub's cookie helper starts with

```js
let o = document.domain;
if (null == o) throw Error("Unable to get document domain");
```

`document` had no `domain` at all, so reading it produced `undefined`, which
is `== null` — the throw took down the whole bundle, after every earlier fix
had let it reach the cookie write.

- `document.domain` = the URL's hostname (no port), from the same parser as
  `location`. Setting it (origin relaxation) is not implemented; nothing in
  the wild has needed it.
- engine tests **986 → 987** green (new `document_domain_is_the_page_host`).
- After this the `core-*.js` module runs to completion. Remaining load
  errors move to other bundles:
  `marketing-essentials`: `Cannot read property 'search' of undefined`,
  `landing-pages`: `undefined is not iterable`.

## `document.location` is the window's Location (P24, 2026-11-10)

The `marketing-essentials` bundle starts with

```js
constructor() { … this.queryString = document.location.search; … }
```

and died on `Cannot read property 'search' of undefined`: `location` existed
as a window global, but `document` carried no `location` property. The spec
aliases `Document.location` to the window's Location object, so the engine now
hands the document the same object.

- engine tests **987 → 988** green (new
  `document_location_is_the_window_location`).
- Next load error, now only in `landing-pages`: `undefined is not iterable`.
  A timer still fails in `pb-*.js`: `Cannot read property 'toUpperCase' of
  undefined`.

## A symbol key survives `in` (P25, 2026-11-10)

With `document.location` in place the landing bundle ran further and died with
`TypeError: undefined is not iterable` inside `@bufbuild/protobuf`:

```js
add(value) {
  if (!(Symbol.for("reflect unsafe local") in value)) …
  this.list.push(value);
}
```

`add` brands the unwrapped message with a symbol key and pushes either the
wrapper or the message depending on the test. The symbol stands in as a plain
object carrying a hidden `__symbol`, and `ToPrimitive` treated it as an
ordinary object: it found no `valueOf`/`toString` that returned a primitive, so
the `in` operator's key became the wrong string (`Symbol(desc)` never reached
`has_member`). The brand check read `false`, the wrapper was stored, and GitHub's
`FileDescriptorProto.messageType` held a `Type` where a `DescriptorProto`
belonged -- the later `for..of` over it then failed with "not iterable".

- `to_primitive` returns the symbol object unchanged, as spec ToPrimitive does
  for a primitive. `s in o` and `o[s] = 1` now land on the same key.
- engine tests **988 -> 989** green (new
  `a_symbol_key_works_with_the_in_operator`).
- Next fatal error: `Uncaught TypeError: Cannot convert object to primitive
  value` (the null-equality rule below).

## `null == object` never coerces the object (P26, 2026-11-10)

Past the symbol fix, the landing bundle threw
`TypeError: Cannot convert object to primitive value` from React's
`__COMPILER_RUNTIME`:

```js
var r = getCachedValue(), m = cache.get(r);
null == r || store(r, m);
```

`r` is a null-prototype object with no `valueOf`/`toString`. Loose equality
ran ToPrimitive on both operands before comparing, so the comparison threw
instead of answering `false`. Per spec `null`/`undefined` compare equal only
to each other; the other operand is never coerced.

- `binary_op`'s `==`/`!=` now short-circuits when either side is `null` or
  `undefined`: true only for the null/undefined pairs, false otherwise, with
  no ToPrimitive call.
- engine tests **989 -> 990** green (new
  `null_equality_does_not_coerce_the_other_side`).
- Next fatal error: `ReferenceError: Proxy is not defined` (below).

## `Proxy` with get/set traps (P27, 2026-11-10)

The landing bundle then died with `ReferenceError: Proxy is not defined`.
Turbo wraps its router API so one name reads differently:

```js
return new Proxy(e, { get: (e, r, n) => "navigate" === r ? t : Reflect.get(e, r, n) })
```

- shortcut: a proxy is a plain object carrying a hidden `__proxy`
  `{ target, handler }` slot. The `get` and `set` traps run when the handler
  has them; everything else forwards to the target: reads and writes without a
  trap, `in`, `delete`, the prototype walk (so `instanceof` and inherited
  methods work). Own-key enumeration sees the proxy itself (no proxy keys), not
  the target. `has`/`deleteProperty`/`getPrototypeOf`/`ownKeys` traps do not
  run.
- `new Proxy` rejects a non-object target or handler with a TypeError, and
  `__proxy` is hidden from enumeration.
- engine tests **990 -> 991** green (new
  `proxy_get_trap_overrides_and_forwards_to_the_target`).
- Next fatal error: `Uncaught TypeError: undefined is not a function` in
  `landing-pages-623b97b89e4b9885.js` (thrown inside
  `wp-runtime-c06c3de1d3ace990.js`).

## `String.prototype.concat` (P28, 2026-11-10)

The next call failure pointed into an `8d-*.js` chunk at 2:17069, and the
callee there is `""`:

```js
var o = "0-9\uFF10...", i = "".concat("-\u2010-\u2015...").concat("\uFF0F/")...
```

That is how the minified bundle writes a long literal: a chain of
`"".concat(part)`. The engine had no `concat` on its string method table, so
the receiver's method was `undefined` and the call threw "undefined is not a
function".

- `String.prototype.concat(...args)` appends the ToString of every argument to
  the receiver; installed on the prototype table and in the primitive dispatch
  so both `"a".concat(...)` and `String.prototype.concat.call(...)` work.
- engine tests **991 -> 992** green (new `string_concat_appends_each_argument`).
- Next fatal error: `Promise.withResolvers is not a function` (below).

## `Promise.withResolvers` (P29, 2026-11-10)

The landing chunk's batching helper starts with

```js
let R = Promise.withResolvers()
```

and the engine's `Promise` had only the five combinators. `withResolvers()`
returns the deferred `{ promise, resolve, reject }`; there is no executor, so
it reuses the same slot natives the `new Promise(executor)` path installs.

- engine tests **992 -> 993** green (new
  `promise_with_resolvers_returns_the_three_parts`).
- Next fatal error: `undefined is not a function` at `Math.asin` in the same
  chunk (below).

## Math trigonometry (P30, 2026-11-10)

The landing chunk's easing curve is

```js
a = s / F * (Math.asin(1 / n) || 0)
```

and `Math` here only had floor/ceil/round/abs/max/min/pow/sqrt/trunc/random/
sign. That chunk alone calls `sin` (8x), `cos` (9x), `tan` (2x), `asin` and
`atan2`, so all of that family was missing.

- `Math.sin/cos/tan/asin/acos/atan` take one value; `Math.atan2(y, x)` uses
  the first argument as Rust's receiver (`y.atan2(x)`), which is the JS order.
- engine tests **993 -> 994** green (new `math_trigonometry_exists`).
- Next fatal error: `e.substr is not a function` (below).

## `String.prototype.substr` (P31, 2026-11-10)

The same chunk names a DOM plugin with

```js
e = ("css" === e ? "CSS" : e.charAt(0).toUpperCase() + e.substr(1)) + "Plugin"
```

`substr` is Annex B, but browsers have it and bundles use it. The engine's
string table had `slice` and `substring` but not `substr`.

- `substr(start, length)` is the length form: a negative start counts from
  the end, the second argument is a count, and an absent count runs to the end
  (`slice` is the end-index form).
- engine tests **994 -> 995** green (new `string_substr_takes_a_length`).
- Next fatal error: the grapheme splitter's regex is rejected with
  `unsupported unicode property escape \p{RI}` (below).

## Emoji Unicode properties in regex (P32, 2026-11-10)

With `substr` in place the landing module got as far as its grapheme splitter:

```js
/\p{RI}\p{RI}|\p{Emoji}(\p{EMod}|\u{FE0F}\u{20E3}?|[\u{E0020}-\u{E007E}]+\u{E007F})?(…)*|./gu
```

`\p{...}` knew only the letter/number/space/ASCII names, so this was a
`SyntaxError` for the whole module.

- `\p{RI}` / `\p{Regional_Indicator}` = U+1F1E6..U+1F1FF,
  `\p{EMod}` / `\p{Emoji_Modifier}` = U+1F3FB..U+1F3FF.
- `\p{Emoji}` is an honest shortcut: the symbol/pictograph blocks (BMP
  symbols 0x2194..0x27BF plus 0x1F000..0x1FAFF and the keycap bases), not the
  full Unicode property. The splitter only groups a cluster and never indexes
  into the matched text, so a slightly wider set changes no page behaviour.
- engine tests **995 -> 996** green (new `regex_emoji_property_escapes`).
- Next fatal error: `bad hexadecimal escape in pattern` for the same regex:
  its tag-sequence class is `[\u{E0020}-\u{E007E}]` (below).

## `\u{...}` at the upper bound of a class range (P33, 2026-11-10)

With the emoji properties accepted, the same pattern still failed:
`bad hexadecimal escape in pattern`. Its class is
`[\u{E0020}-\u{E007E}]`; the atom path and the range's *lower* bound both
understood the brace form, but the upper-bound branch only read `\uXXXX`, so
the `{` was "not a hex digit".

- one `unicode_escape` helper now reads `\uXXXX` or `\u{...}` (lone
  surrogates still map to U+FFFD) and all three call sites use it.
- engine tests **996 -> 997** green (new
  `regex_unicode_escape_at_class_range_bound`).
- github.com cold load: **zero `script error in` lines**. The only remaining
  error is the recurring timer failure in `pb-*.js`
  (`Cannot read property 'toUpperCase' of undefined`, 35:26875).

## `nodeName` on element wrappers (P34, 2026-11-10)

The last repeated failure on github.com was a timer in `pb-*.js`:

```js
n.prototype.indexes.push({ name: "TAG", selector: e => e.match(c) && e.match(c)[0].toUpperCase(),
  element: e => [e.nodeName.toUpperCase()] })
```

That is Sizzle's tag index. Element wrappers exposed `tagName` but no
`nodeName`, so `e.nodeName` was `undefined` and `.toUpperCase()` threw every
time the selector engine indexed the page.

- `nodeName` mirrors `tagName` on an element wrapper (both are the parsed,
  upper-case tag name).
- engine tests **997 -> 998** green (new `elements_have_a_node_name`).
- github.com cold load now prints **no `script error` and no `timer error`
  lines at all**. The remaining console noise is a custom-element constructor
  throw for `<relative-time>` (`attachShadow` is missing) and two
  `addEventListener: callback is not a function` notes.

## `attachShadow` / `shadowRoot` (P35, 2026-11-10)

With Sizzle silent, the only error on github.com was every custom element
constructor:

```
custom element <relative-time>: constructor threw
Uncaught TypeError: Cannot read property 'call' of undefined (in define <- D)
at 136:133524 of pb-f417da738ec82369.js
```

That line is the command polyfill (the one that turns `[oncommand]` into
`CommandEvent`):

```js
e = globalThis.HTMLElement || function(){},
i = e.prototype.attachShadow,
e.prototype.attachShadow = function (init) { let n = i.call(this, init); return t(n), n }
```

`i` was `undefined`: the engine had no `attachShadow` anywhere, and the
prototype is what the polyfill reads. `t(root)` then does
`root.addEventListener("click", …)`, `root.querySelectorAll("[oncommand]")`.

- `Element.prototype.attachShadow(init)` now answers a real `#fragment` arena
  node (nodeType 11) cached per host in `SHADOW_ROOTS`, so `shadowRoot` recalls
  the same root. The returned wrapper also carries `host` and `mode`.
- the root is a fragment, so the ordinary insertion and query methods work on
  it; `attachShadow` is on `Element.prototype` (inherited by
  `HTMLElement.prototype`), exactly where the polyfill looks.
- engine tests **998 -> 999** green (new `attach_shadow_returns_a_fragment_root`).
- github.com cold load: the `<relative-time>` constructor error is gone. The
  only remaining console lines are two
  `addEventListener: callback is not a function` notes (next entry).

## `EventListener` objects (P36, 2026-11-10)

The last two console notes on github.com were, after adding the call site to
the message:

```
window.addEventListener: callback is not a function
  (test object (at 35:18887 of pb-*.js))
window.addEventListener: callback is not a function
  (test object (at 1:21838 of react-dom-client-*.js))
```

Both are the same feature detection, in Lit and in React:

```js
var probe = {};
Object.defineProperty(probe, "passive", { get: function () { supported = true } });
window.addEventListener("test", probe, probe);
window.removeEventListener("test", probe, probe);
```

The DOM's `EventListener` may be an object with a `handleEvent` method, so a
browser accepts this silently. The engine only accepted functions, logged the
note, and skipped the registration.

- `addEventListener` (element and window) accepts an object listener as well as
  a function; primitives still get the diagnostic.
- dispatch looks `handleEvent` up at invoke time and calls it with the event,
  which is the spec's callback-interface rule.
- engine tests **999 -> 1000** green (new
  `add_event_listener_accepts_a_handle_event_object`).
- github.com cold load now prints **no `script error`, no `timer error`, no
  custom-element constructor throw and no callback note**. What remains is the
  documented observer/relative-time noise only (IntersectionObserver and
  ResizeObserver never fire).

## `connectedCallback` (P37, 2026-11-10)

With every earlier error gone, github.com's first *dynamic* failure appeared:
custom elements construct fine now, but nothing ever tells them they are in the
document. Lit renders from `connectedCallback`, so `<tool-tip>` and
`<relative-time>` were upgraded and then stayed empty:

```
custom element <tool-tip>: connectedCallback threw
Uncaught TypeError: Cannot read property 'getElementById' of undefined
```

The first throw was the *inside* of a callback that had never run before: the
engine upgraded the element, set `CE_INSTANCES`, and stopped there. The spec
"upgrade" and "insert" steps both enqueue a `connectedCallback` once the node
is in a document.

- `DomHost::is_connected` walks parent links and checks membership, so an
  orphan (parent 0 but not listed among the root's children) is not connected.
- `upgrade_custom_element` now follows the constructor with
  `connect_custom_element`, which fires `connectedCallback` only for a
  connected element, deduped by `CE_CONNECTED`.
- `insert_children`, `appendChild` and `insertBefore` call `connect_subtree`,
  so a detached subtree only connects when it is actually inserted.
- one note per page if a callback throws (`CE_LIFE_FAILED`).
- `disconnectedCallback` is still not fired on removal, so re-insertion does
  not run `connectedCallback` again (documented gap).
- engine tests **1000 -> 1001** green (new
  `custom_element_connected_callback_fires_once_connected`).
- github.com cold load: the callback now runs and fails one line further in,
  at `this.ownerDocument.getElementById(...)` — `ownerDocument` was undefined
  (next entry). Script exec time rose from 5.0 s to 6.3 s because the
  callbacks are now doing real work.

## `ownerDocument` (P38, 2026-11-10)

The first `connectedCallback` to run threw immediately:

```
custom element <tool-tip>: connectedCallback threw
Uncaught TypeError: Cannot read property 'getElementById' of undefined
(at 136:402 of pb-*.js)
```

That column is the `<tool-tip>` `control` getter:

```js
get control() { return this.ownerDocument.getElementById(this.htmlFor); }
```

`ownerDocument` existed nowhere on the wrapper, so the lookup itself threw
before `getElementById` was even reached.

- `DomHost::node_owner_document` isn't needed: every node in this arena belongs
  to the one document, which is the global `document` object. The getter
  (`E_OWNER_DOCUMENT`) returns `globalThis.document`, so
  `el.ownerDocument === document` is true.
- exposed on element wrappers and on character data (text/comment) wrappers.
- engine tests **1001 -> 1002** green (new `nodes_have_an_owner_document`).
- github.com cold load: the `<tool-tip>` callback completes; the next throw is
  `<relative-time>`'s `Date.parse` (next entry).

## `Date.parse` (P39, 2026-11-10)

`<tool-tip>` connected, and `<relative-time>` took its place:

```
custom element <relative-time>: connectedCallback threw
Uncaught TypeError: undefined is not a function (at 223:14589 of pb-*.js)
```

The reported site is the `datetime` getter, but the failing call is one
statement later in the same `get date()`:

```js
get date(){ let e = Date.parse(this.datetime); return Number.isNaN(e) ? null : new Date(e); }
```

`Date.parse` sent `get_member` to a miss, so the call itself was the
"undefined is not a function". (`call_span` is set before the arguments
evaluate, so a nested call can be reported under the wrong column.)

- `Date.parse` is a `Meth::DateParse` arm, reached through `get_member`'s
  native `"parse"` name, the same dispatch style as `Date.now`.
- the parser covers the ISO-8601 forms pages actually write: `YYYY`, `YYYY-MM`,
  `YYYY-MM-DD`, each optionally with `THH:mm[:ss[.fff]]` and `Z` / `±HH:MM` /
  `±HHMM`; `24:00` rolls into the next day; an out-of-range field (`2020-02-30`)
  is NaN. Anything outside those forms is NaN rather than a best-effort guess.
- a date-time with no offset is local in the spec; this engine has no local
  zone, so it reads UTC (documented shortcut).
- `new Date(string)` uses the same parser instead of yielding NaN.
- engine tests **1002 -> 1003** green (new `date_parse_reads_iso_forms`).
- github.com cold load: `Date.parse` returns, and the same getter now fails on
  the *next* call in that line — `Number.isNaN` is missing (next entry).

## `Number.isNaN` (P40, 2026-11-10)

`<relative-time>`'s `date` getter has two calls on one line, and the second one
was missing too:

```js
get date(){ let e = Date.parse(this.datetime); return Number.isNaN(e) ? null : new Date(e); }
```

`Number` only carried its value constants, so `Number.isNaN` was undefined and
the getter threw again.

- `Number.isNaN` is the spec's strict form: true only for a Number that is NaN,
  so `Number.isNaN("NaN")` and `Number.isNaN()` are false (the global `isNaN`
  still coerces).
- reached through a new `n_num_globals` index, installed as an own property of
  the `Number` native.
- engine tests stay at **1003** (the new assertions extend the existing
  `primitive_and_function_prototypes_exist` test rather than adding one).
- github.com cold load: `<relative-time>` connects and renders; the next throw
  is `<react-app>`'s `connectedCallback` calling `toggleAttribute` (next
  entry).

## `Element.toggleAttribute` (P41, 2026-11-10)

`<react-app>` (GitHub's Catalyst controller) then became the first throw:

```
custom element <react-app>: connectedCallback threw
Uncaught TypeError: undefined is not a function (at 136:145728 of pb-*.js)
```

The column is the loop head of:

```js
connectedCallback(e, t) {
  for (let t of (e.toggleAttribute("data-catalyst", true),
                 customElements.upgrade(e),
                 e.querySelectorAll("template[data-shadowroot]")))
    …
```

`customElements.upgrade` was already a callable no-op and `querySelectorAll`
existed, so the undefined call was `toggleAttribute` itself.

- `Element.toggleAttribute(name, force)` implements the spec's three cases:
  present with no force or `false` → remove and return false; absent with no
  force or `true` → add with an empty value and return true.
- exposed on element wrappers and on `Element.prototype` (like `hasAttribute`).
- engine tests **1003 -> 1004** green (new
  `toggle_attribute_flips_presence_and_returns_the_result`).
- github.com cold load: the Catalyst callback now proceeds and fails on
  `No embedded data provided for react element`, a *page* error whose cause is
  the selector matcher (next entry).

## `querySelector` uses the cascade's selector matcher (P42, 2026-11-10)

With `toggleAttribute` in place the `<react-app>` callback ran its first
statement and then threw an error the *page* raises, not the engine:

```
custom element <react-app>: connectedCallback threw
Uncaught Error: No embedded data provided for react element
```

Catalyst resolves its `@target` properties with a selector query, and the
element it found nothing for is right there in the HTML:

```html
<react-app app-name="landing-pages" data-ssr="true">
  <script type="application/json" data-target="react-app.embeddedData">{…}</script>
</react-app>
```

```js
function P(e, t) { Object.defineProperty(e, t, { get() { return h(this, t) } }) }
// h queries `[data-target~="<tag>.<name>"]` inside the host
```

The script-side matcher (`querySelector`, `querySelectorAll`, `matches`,
`closest`) only understood a bare tag, `#id` or `.class`; every attribute
selector silently matched nothing.

- `css::Sel` exposes the cascade's own parser and matcher, and the script-side
  functions compile once per query and reuse it. `[attr]`, `[attr=v]`,
  `[attr~=v]`, `[attr|=v]`, `[attr^=v]`, `[attr$=v]`, `[attr*=v]`, compound
  sequences, pseudo-classes and combinators now behave the same in
  `querySelectorAll` as in a style rule.
- `getElementById` and element-scoped `getElementById` no longer build a `#id`
  selector: ids are matched literally, because React's streaming markers use
  ids like `B:0` that are not valid CSS (the shared matcher would read `:0` as
  a pseudo-class and stop matching). This was caught by
  `comment_and_sibling_navigation`.
- engine tests **1004 -> 1005** green (new
  `query_selector_supports_attribute_selectors`).
- github.com cold load: the embedded JSON is found and React starts running;
  the next throw is `crypto is not defined`. Script exec time rose to 17.6 s
  because the page's React tree now actually mounts.

## `crypto.randomUUID` (P43, 2026-11-10)

With the selector matcher fixed, React mounted far enough to mint its first
element id:

```
custom element <react-app>: connectedCallback threw
Uncaught ReferenceError: crypto is not defined
```

- a `crypto` global with the two calls pages actually make:
  `getRandomValues(typedArray)` (fills the view in place and returns it) and
  `randomUUID()` (RFC 4122 version 4: 16 random bytes with the version and
  variant bits set).
- the bytes come from a new `JsEngine::random_u32`, the same xorshift as
  `Math.random` — a PRNG, not a CSPRNG. shortcut: real entropy is not a
  browser-engine feature and nothing on a page can observe the difference.
- engine tests **1005 -> 1006** green (new
  `crypto_random_uuid_and_get_random_values`).
- github.com cold load is now **completely clean**: no `script error`, no
  `timer error`, no custom-element constructor or callback throw, no callback
  note. The page renders 2681 elements and 56 images; script exec ~15.7 s
  (fetch 2.2 s) with the React tree mounted for real.

## `\p{Bidi_Control}` (P44, 2026-11-10)

The landing page was clean, so the next measurement moved one click in, to a
repository page (`https://github.com/rust-lang/rust`). There the *code view*
bundle is fetched, and `dp-*.js` failed to compile:

```
[js] script error in code-view-*.js: dp-*.js:
SyntaxError: /[\p{Bidi_Control}\u0590-\u08ff…]/u:
unsupported unicode property escape \p{Bidi_Control} (line 2, col 15379)
```

GitHub classifies bidirectional-formatting controls in a displayed source file
with exactly that character class; an unknown property name is a *compile*
error, so the whole code-view module (and therefore every file blob) was dead.

- `PropKind::BidiControl`, accepted as `\p{Bidi_Control}` and the short
  `\p{Bidi_C}`.
- the ranges are exact rather than approximate (UAX #9): `U+061C`, `U+200E`,
  `U+200F`, `U+202A..=U+202E`, `U+2066..=U+2069`. Notably `U+200B` zero-width
  space is *not* a bidi control, and the test pins that.
- engine tests **1006 -> 1007** green (new `regex_bidi_control_property_escape`).
- `github.com/rust-lang/rust`: the code-view module now compiles and fails one
  step later, on a *lookbehind* in `dp-*.js` (next entry).

## Regex lookbehind (P45, 2026-11-10)

With `\p{Bidi_Control}` known, the code-view module compiled and the *next*
bundle in that chain reported the next gap:

```
[js] script error in code-view-*.js: dp-*.js:
SyntaxError: /^(?!-).*(?<!-)$/: lookbehind is not supported (line 2, col 15379)
```

GitHub uses it to reject a line that starts or ends with a dash. The parser
recognised `(?<=` / `(?<!` only to reject them.

- `Node::LookBehind(negate, body)` compiles to a new `Inst::LookBack`, the
  same shape as the lookahead `Inst::Look` (body range + `SubEnd` sentinel).
- the VM's sub-run gained an `end_at: Option<usize>` constraint. For a
  lookbehind the body is run from each candidate start left of the current
  position (nearest first) and only a path that lands *exactly* on the
  assertion's position counts — that is what makes `(?<=ab)` fail on `"bc"`,
  and letting the constraint live inside the VM means the body still gets its
  own backtracking instead of accepting the first early success.
- a positive lookbehind adopts the sub-run's captures (JS semantics); a
  negative one discards them. `MAX_LOOK_DEPTH` and the shared step budget
  bound the extra work exactly as they do for lookahead.
- shortcut: when the body itself is ambiguous (`(?<=(a+))`) the group comes
  from the nearest successful start, where a browser's right-to-left run
  prefers the greedy one. Flagged here, not silently different.
- engine tests **1007 -> 1008** green (new `regex_lookbehind`, 10 assertions
  including the GitHub pattern, the exact-end rule, a `\d{2}` body and
  `split(/(?<=-)/)`). `regex_unsupported_syntax_is_clean_error` now pins
  `(?|` as its representative unknown form instead of lookbehind.
- `github.com/rust-lang/rust`: `dp-*.js` compiles; the module chain now stops
  on `\p{Mn}` (next entry). Script exec rose from 8.8 s to 12.2 s because the
  code view is running.

## Mark property escapes (P46, 2026-11-10)

The next bundle in the same chain needed the mark categories:

```
[js] script error in code-view-*.js: dp-*.js:
SyntaxError: /\p{Mn}/u: unsupported unicode property escape \p{Mn} (line 2, col 15473)
```

That module classifies source text for its homograph/confusable check and
declares all four forms: `\p{Mn}`, `\p{Mc}`, `\p{Me}` and the union
`[\p{Mn}\p{Mc}\p{Me}]`.

- `PropKind::Mark`, accepted as `M` / `Mark` / `Mn` / `Nonspacing_Mark` / `Mc`
  / `Spacing_Mark` / `Me` / `Enclosing_Mark`.
- shortcut: the engine has no Unicode tables, so the three subcategories are
  **not distinguished**. They all answer from `MARK_RANGES`, a block list of
  the combining marks by script (combining diacritics, Cyrillic, Hebrew,
  Arabic, the Indic scripts, Thai/Lao, Tibetan, Myanmar, Khmer, the
  combining supplement, variation selectors, and the supplementary-plane
  blocks). Block granularity over-accepts a few neighbours in the same block;
  every real combining mark in those scripts is covered. Flagged here rather
  than pretending to be exact.
- engine tests **1008 -> 1009** green (new `regex_mark_property_escapes`,
  including the union form and the "letters are not marks" side).
- `github.com/rust-lang/rust`: the mark checks compile; the module chain now
  stops on `\p{Script=Greek}` (next entry).

## `\p{Script=…}` (P47, 2026-11-10)

With the marks known, the same confusable check named scripts directly:

```
[js] script error in code-view-*.js: dp-*.js:
SyntaxError: /\p{Script=Greek}/u: unsupported unicode property escape \p{Script=Greek}
```

The module groups Greek, Hebrew, Arabic, Syriac, Thaana, Mandaic, Hiragana,
Katakana and Han characters, and GitHub's own source files carry all nine.

- `prop_kind` splits on `=` first: `Script` / `sc` / `Script_Extensions` /
  `scx` go to `script_kind`, which accepts the long name and the ISO 15924 tag
  for those nine scripts. A property name with `=` is never a binary property,
  so the existing table is untouched.
- shortcut: script membership is the script's Unicode **block** (plus the
  presentation-form and supplementary blocks that script actually uses), not
  the per-character Script property. Block granularity over-accepts a few
  shared-block neighbours; every code point of the nine scripts is covered.
- exactly nine scripts, not the full Unicode list: an unlisted value is still
  an honest compile error, and the test pins that with `\p{Script=Ogham}`.
- engine tests **1009 -> 1010** green (new `regex_script_property_escapes`:
  one character per script, the Latin negative, the `sc=` / `scx=` short forms
  and the `\P{…}` negation).
- `github.com/rust-lang/rust`: the code-view module compiles and the protobuf
  descriptor builder inside `4u-*.js` runs for real for the first time
  (next entry).

## Map `values()` yields values (P48, 2026-11-10)

With the regex gaps closed, GitHub's protobuf descriptor builder in `4u-*.js`
ran and threw inside its message walk:

```
[js] script error in code-view-*.js: Uncaught TypeError: Cannot read
property 'oneofDecl' of undefined (in Q <- q <- et)
```

`et` builds one `oneof` per `DescriptorProto.oneofDecl` entry and got
`undefined` entries because the thing it walked was an *array*: the builder
collects descriptors with `for (let e of s.values())` over a `Map`, and the
engine's values iterator handed back the raw `[key, value]` pair, which is what
`entries()` yields.

- `iter_step` unwraps the pair for `IterKind::Values` when the source carries
  the hidden `__map` prop. Sets keep the value itself under `__set`, so they
  fall through untouched, and `entries()` still gets the pair.
- engine tests **1010 -> 1011** green (new
  `map_values_iterator_yields_values_not_pairs`: values order, keys, entries,
  the default iterator staying `entries`, a `for...of` sum, and the Set side).
- `github.com/rust-lang/rust`: the descriptor walk proceeds; the next failure
  is that every *string* in the decoded descriptor is empty (next entry).

## TextDecoder reads typed-array views (P49, 2026-11-10)

The descriptor walk then reported what looked like a page-side inconsistency:

```
[js] script error in code-view-*.js: Uncaught Error:
invalid FieldDescriptorProto: type_name .github.common.v1.SafeHtml not found
```

It was ours. Instrumenting the bundle's own field builder showed field
descriptors with `type: 11` and an **empty** `typeName`; a probe inside
`4u-*.js` then showed that every length-delimited field (name, type name,
json name) decoded as `""` while the varint fields (`number`, `label`, `type`)
were correct. `@bufbuild/protobuf` decodes those strings with

```js
decodeUtf8: (t, a) => a ? fatal.decode(t) : new globalThis.TextDecoder().decode(t)
```

where `t` is always a `Uint8Array` or a view into one. The engine's
`TextDecoder.prototype.decode` accepted only a *plain array* of byte values, so
anything else became `""`.

- the decode branch goes through `text_bytes`, which understands a view's byte
  window (`offset .. offset + len * size`, so `subarray()` decodes just its
  slice), a whole `ArrayBuffer`, and a plain array. A `Float32Array` /
  `Int16Array` view therefore decodes its underlying bytes rather than one char
  per element.
- engine tests **1011 -> 1012** green (new
  `text_decoder_reads_typed_array_views`: a `Uint8Array`, `subarray()` with and
  without an end, an `ArrayBuffer`, multibyte UTF-8, a round trip through
  `TextEncoder`, and a non-byte view).
- diagnostic note: this hunt hand-patched the `4u-*.js` disk-cache entry twice
  (a `FDBG`/`GM`/`QF`/`AF` probe plus a refreshed timestamp so the cache did
  not re-download over it). The patched file was deleted afterwards, so no
  debug copy ever shipped.
- `github.com/rust-lang/rust`: the descriptor strings decode, so the builder
  names the type it wants (`type_name .github.common.v1.SafeHtml`); the lookup
  still missed, because the file walker that should register it was broken
  (next entry).

## `yield` inside a `switch` (P50, 2026-11-10)

Empty strings were not the only reason for `type_name .github.common.v1.SafeHtml
not found`. The registry's file walker is a generator whose cases yield:

```js
function* e(t){switch(t.kind){
  case "file": for(let n of t.messages) yield n, yield* e(n); yield*t.enums, …
  case "message": …}}
```

and its switch body never ran. A direct probe confirmed it: `function*
g(){ switch (1) { case 1: yield 7 } }` returned a generator that was already
done, and even a case body with no yield in it was skipped.

- `step_stmt`'s switch arm pushed only the `SwitchBody` marker frame (the
  `break` target) and never the batch of stitched case bodies. The marker's
  own step reported "finished" immediately, so the body was dropped. Every
  `yield` inside a switch had silently done nothing since the walker was
  written; nothing had measured it before.
- the arm now pushes the marker *and* the batch above it, exactly like a loop
  body. Fall-through runs the batch to its end and the marker then reports
  finished; `break` unwinds to the marker and pops it (`unwind_flow` already
  treated it as a break target, so that side was already right).
- a bare `{ … }` block statement holding a yield hit the walker's "yield in
  this statement position" error; it now gets a batch of its own with the
  block's own scope.
- engine tests **1012 -> 1013** green (new `generator_yield_inside_switch`,
  seven assertions: one case, fall-through, `break` plus a non-matching case,
  the resume value of `let x = yield`, `default:` as the entry case, a bare
  block, and a statement after the switch).
- `github.com/rust-lang/rust`: the dependency file's messages register, so the
  descriptor walk gets through and the module chain stops on `Intl is not
  defined` (next entry).

## `Intl` (P51, 2026-11-10)

With the descriptor walk fixed, the code-view module failed at module scope:

```
[js] script error in code-view-*.js: Uncaught ReferenceError: Intl is not defined
```

It builds three `Intl.DateTimeFormat` instances at module scope (blame dates:
`{year:"numeric",month:"short",day:"numeric",hour:"numeric",minute:"numeric",
timeZoneName:"short"}`, `{year,month,day}`, `{month,day}`) and one
`Intl.NumberFormat("en-US",{notation:"compact",maximumFractionDigits:1,
maximumSignificantDigits:2})` for dependent counts.

- `Intl.DateTimeFormat` / `Intl.NumberFormat` are ordinary constructors whose
  instances are plain objects pointing at their prototype; the options live
  under a hidden `__intl` own prop, and `format` reads them back.
- shortcut: en-US only, no locale data and no timezone database. Month and
  weekday names are the English ones, `timeZoneName` answers "UTC", and a
  date-time is UTC. Compact notation rounds to `maximumSignificantDigits` and
  appends K/M/B/T.
- `date_parts` now shares the civil-date math with `toISOString`.
- engine tests **1013 -> 1014** green (new
  `intl_date_time_and_number_format`: the three GitHub date shapes, the default
  numeric date, the PM clock, grouping, and compact 1.2K / 1.5M / 999 / 12K).
- `github.com/rust-lang/rust`: code-view gets past `Intl`; script exec
  32133 -> 25132 ms, worker 101038 -> 61545 ms. The next failure is the
  selector observer's `rootNode.contains` (next entry).

## `Node.contains` (P52, 2026-11-10)

The next error was in `core-*.js`:

```
[js] mutation observer error: Uncaught TypeError: undefined is not a function
  (at 136:117612 of core-*.js)
```

`core`'s selector observer checks `e.rootNode.contains(i)` before it attaches
to a node; `contains` was on no wrapper, so `undefined` was called and the
observer never attached to a single selector.

- `DomHost::node_contains(root, other)` walks `other`'s ancestor chain and
  checks each parent link's child list, so an orphan that merely points at the
  document root is not "contained". Bounded like `closest`.
- `Node.contains` is installed on every element wrapper; `document.contains`
  uses arena node 0. A non-node argument answers false instead of throwing.
- engine tests **1014 -> 1015** green (new `node_contains_walks_the_arena`:
  self, child, grandchild, sibling, upward, document, orphan, non-node).
- `github.com/rust-lang/rust`: no mutation-observer error; script exec
  25132 -> 9813 ms, worker 61545 -> 37207 ms. The next failure is `String.raw`
  in `e95-*.js` (next entry).

## `String.raw` (P53, 2026-11-10)

The code-view module then threw from `e95-*.js`:

```
[js] script error in code-view-*.js: Uncaught TypeError: undefined is not a
function (at 2:13967 of e95-*.js)
```

The reported column lands inside a template literal because the callee is the
tag:

```js
i = RegExp(String.raw`\b(?:${words.split(" ").join("|")}):\S*`, "gi")
```

`String.raw` was the only static missing from `String`.

- `String.raw(strings, ...subs)` uses `strings.raw` when present, else
  `strings`, and interleaves the substitutions. The tagged-template form works
  because `tagged_args` already gives the strings array a `raw` array.
- shortcut: that `raw` array still holds the *cooked* text (the parser keeps no
  original escape spelling), so ``String.raw`\n` `` differs from a real engine
  for escapes. Enough for GitHub's filter regex, which is built from plain
  words; noted rather than hidden.
- engine tests stay **1015** green (four `String.raw` assertions added to
  `primitive_and_function_prototypes_exist`: the array form, a single segment,
  a tagged template, and an explicit substitution).
- `github.com/rust-lang/rust`: code-view loads; script exec 9813 -> 1594 ms,
  worker 37207 -> 2532 ms. The next failure is in `dp-*.js` at 2:997 (next
  entry).

## Strict `Number` statics (P54, 2026-11-11)

With `String.raw` in place the code-view module loaded far enough to reach
GitHub's predicate library, which threw at `dp-*.js` 2:997:

```js
function s(e){return Number.isFinite(e)}
```

That column is the `Number.isFinite(e)` call itself. The library also calls
`Number.isInteger`; only `Number.isNaN` existed on the constructor.

- `Number.isInteger`, `Number.isFinite` and `Number.isSafeInteger` joined the
  existing `n_num_globals` native (idx 5/6/7). All three are strict: a non-number
  argument answers false, so `Number.isFinite("3")` is false and
  `Number.isInteger(NaN)`/`(Infinity)` are false. An integer is a finite number
  with no fractional part; a safe integer is also within ±2^53-1.
- `Number.parseInt`/`Number.parseFloat` are the *same* function objects as the
  globals, which is what the spec says and what `===` checks in the wild expect.
- engine tests stay **1015** green (ten assertions added to
  `primitive_and_function_prototypes_exist`: strictness for all three, the
  fractional and 2^53 boundaries, and the identity of the parse aliases).
- `github.com/rust-lang/rust`: the error moves past 2:997 to `dp-*.js` 2:2524
  (`Object.getOwnPropertySymbols`, next entry). Exec time rises
  1594 -> 25596 ms and the worker 2532 -> 64005 ms, because the module chain now
  gets much further before it stops.

## `Object.getOwnPropertySymbols` (P55, 2026-11-11)

The predicate library defines one function per reflection helper, and the next
one in the list had no native behind it:

```js
function Z(e){return Object.getOwnPropertySymbols(e)}
```

- The engine has no primitive symbol: a symbol is a plain object with a hidden
  `__symbol` description, and a computed key written with one lands in the
  property list as its `"Symbol(desc)"` string. This static reads those keys
  back and rebuilds a symbol value per key, so `typeof` is `"symbol"`,
  `String(s)` is `"Symbol(k)"` and `Array.prototype` reports its
  `Symbol.iterator` prop.
- shortcut: the rebuilt value is a fresh stand-in, not the original object, so
  `Object.getOwnPropertySymbols(o)[0] === s` is false; only the key string
  survives a write. A plain string key that happens to spell `Symbol(x)` cannot
  be told apart from a real symbol key - the same bridge the `in` operator
  already crosses.
- engine tests **1015 -> 1016** green (new
  `get_own_property_symbols_reads_symbol_keys_back`).
- `github.com/rust-lang/rust`: **no script error at all** for the first time -
  the module chain runs to the end of the page's bundles. Script exec
  25596 -> 25192 ms, worker 64005 -> 62660 ms; the remaining time is the page's
  own hydration work rather than a stop, so the next measurements are latency,
  not missing builtins.

## `\p{Connector_Punctuation}` (P56, 2026-11-11)

The next page tried was the issue list
(`https://github.com/rust-lang/rust/issues`), whose `structured-issues-*.js`
was a SyntaxError before a single statement ran:

```
SyntaxError: /[^\p{Letter}\p{Mark}\p{Number}\p{Connector_Punctuation}\p{Emoji}]/gu:
unsupported unicode property escape \p{Connector_Punctuation} (line 2, col 7389)
```

That class is GitHub's "word character" test for filter tokens.

- `\p{Pc}` / `\p{Connector_Punctuation}` is one of the few categories the engine
  can answer exactly: Pc is a closed list of ten code points (U+005F, U+203F,
  U+2040, U+2054, U+FE33, U+FE34, U+FE4D..U+FE4F, U+FF3F). The `char`
  predicates have no connector test, so the list is spelled out.
- engine tests **1016 -> 1017** green (new
  `regex_connector_punctuation_property_escape`: underscore, an undertie, the
  fullwidth low line, the two negatives, and the bundle's own class).
- `github.com/rust-lang/rust/issues`: structured-issues parses and runs; the
  only script error left is the repo module's `new Date().getFullYear()`/`getDate`
  call (next entry). Worker 17485 -> 32930 ms on the day's link (74 KB/s to
  githubassets - see the note at the end of this entry), so the timing is not
  the signal; the error moving is.

### A note on the remaining wall-clock time

`curl` against `githubassets.com` from this host measures 74 KB/s and ~1 s to
first byte, so the page's ~3.5 MB of CSS/JS costs ~25 s of *network* no matter
what the engine does. A warm-cache run of the same page executes the same
modules in ~3.2 s of script time (core 0.6-2.1 s, code-view 1.0-1.1 s). That is
why the entries below quote error movement rather than total page time.

## Date getters and `toLocale*` strings (P57, 2026-11-11)

The repo module on the issues page threw at `58-*.js` 2:1286:

```js
value: () => {
  let e = new Date;
  return `${e.getFullYear()}-${`0${e.getMonth()+1}`.slice(-2)}-${`0${e.getDate()}`.slice(-2)}`
},
displayName: new Date().toLocaleDateString("en-us", {year:"numeric",month:"long",day:"numeric"})
```

A `Date` object answered only `getTime`/`valueOf`/`toISOString`.

- The eight getters (`getFullYear`, `getMonth`, `getDate`, `getDay`, `getHours`,
  `getMinutes`, `getSeconds`, `getMilliseconds`) and their `getUTC*` aliases are
  new `Meth` variants dispatched off the Date object, all reading `date_parts`
  (which already backs `toISOString` and `Intl`). An invalid date answers NaN
  from every one of them, as the spec says.
- shortcut: there is no timezone database, so local time IS UTC - the same
  shortcut `Intl.DateTimeFormat` already takes - which is why the `getUTC*`
  aliases answer the same numbers.
- `toLocaleDateString` / `toLocaleTimeString` / `toLocaleString` reuse the
  `Intl.DateTimeFormat` formatter. Per `ToDateTimeOptions` the en-US defaults
  (numeric date; clock time with seconds; both) are filled in only when the
  caller asked for **none** of the components, so `{hour,minute}` prints no
  seconds while the bare call does.
- engine tests **1017 -> 1018** green (new `date_getters_and_locale_strings`:
  all eight getters, the UTC aliases, two NaN cases, the three default shapes,
  and GitHub's own options).
- `github.com/rust-lang/rust/issues`: the repo module's Date error is gone; the
  only script error left is the filter-token regex's
  `\p{Extended_Pictographic}` (next entry).

## `\p{Extended_Pictographic}` (P58, 2026-11-11)

Once the Date getters landed, the issues page failed in `repo-*.js` on the
filter-token regular expression:

```
/([\s,"]*?)((?:>|<|>=|<=)*[\w\d\-_"'?/\.=+!@#$%^&*);:\p{Extended_Pictographic}]*)$/gu
```

The `u` flag makes the unknown property name a SyntaxError for the whole
module.

- `PropKind::ExtendedPictographic` answers from `is_emoji` minus the ASCII
  keycap bases (`#`, `*`, `0..=9`, U+20E3): those code points are Emoji but not
  Extended_Pictographic. Both the long name and `ExtPict` are accepted.
- shortcut: the answer is block-granular, the same approximation `Emoji`
  already uses, so a few unassigned code points inside an emoji block report
  true.
- engine tests **1018 -> 1019** green (new
  `regex_extended_pictographic_property_escape`: an emoji pair, U+00A9, U+2764,
  the two keycap exclusions, and the bundle's own class compiling).
- `github.com/rust-lang/rust/issues`: the SyntaxError is gone; the next script
  error is `xf-*.js` reading `Object.prototype.constructor.toString()`.

## The prototype `constructor` back-pointer (P59, 2026-11-11)

With the pictograph escape in place the same page reached `xf-*.js` (Immer) and
threw at 2:437:

```js
var f = Object.prototype.constructor.toString();
```

Our built-in prototypes exposed their methods but not the spec's
`constructor` back-pointer, so the property was `undefined`.

- `link_ctor` stamps `constructor` onto every built-in prototype object as it is
  installed: `Object`, `Array`, `Function`, `String`, `Number`, `Boolean`,
  `RegExp`, `Map`, `Set`, `WeakMap`, `WeakSet`, `Symbol`, `Promise`, `URL`,
  `URLSearchParams`, `ArrayBuffer`, `DataView`, `TextEncoder`, `TextDecoder`,
  `AbortSignal`, `AbortController`, and the two `Intl` formatters. The Error
  family already had one.
- `Object.prototype.constructor === Object` also fixes Immer's second use:
  `Object.hasOwnProperty.call(t, "constructor")` is now true for built-in
  prototype objects, so it no longer decides every plain object is
  non-draftable.
- shortcut: a built-in constructor is a plain callable object, so
  `Object.prototype.constructor.toString()` answers `[object Object]` rather
  than `function Object() { [native code] }`. The call now succeeds, which is
  what the bundle needs; the string itself is not compared to Chrome's.
- engine tests **1019 -> 1020** green (new
  `builtin_prototypes_carry_their_constructor`: 13 identity checks, the Error
  subclass, the own-property form, `[].constructor`, and the bundle's exact
  expression).
- `github.com/rust-lang/rust/issues`: **no script errors** for the first time
  (worker 16084 -> 13292 ms across the same-run asset set; the network note
  above still dominates). The repo page keeps its zero-error run.

## `[^]` is the class of every character (P60, 2026-11-11)

`github.com/search?q=test&type=repositories` reached the search bundle
`kn-*.js` and failed to parse:

```
/^'([^]*?)'?$/
```

reported as `unterminated character class`.

- The class parser refused `]` while the class was still empty, so it consumed
  the `]` of `[^]` as a literal and then ran off the end. In ECMAScript a `]`
  closes the class wherever it appears, which makes `[]` the empty class and
  `[^]` the negation of it: every character, line breaks included. `[^]` is
  the standard "any character" idiom precisely because `.` excludes newlines.
- a literal `]` at the start of a class must be escaped (`[\]]`), as the spec
  says; the old behavior accepted the unescaped form, so this is a deliberate
  deviation removed rather than added.
- engine tests **1020 -> 1021** green (new
  `regex_empty_and_negated_empty_class`: `[^]` on a letter and on a line
  break, the bundle's own `^'([^]*?)'?$` over a multi-line string, `[]` never
  matching, and `[\]]`).
- `github.com/search?q=test&type=repositories`: the SyntaxError is gone; the
  next script error is `ws-*.js` binding `BigInt.prototype.toString`.

## `BigInt.prototype` (P61, 2026-11-11)

The search page then threw at `ws-*.js` 2:2119, in DOMPurify's uncurryThis
table:

```js
M = "u" < typeof BigInt ? null : H(BigInt.prototype.toString)
```

`H` binds the function so it can be applied to a bigint value. `BigInt`
existed as a callable native with `asIntN`/`asUintN`, but had no `.prototype`,
so the property read threw.

- `BigInt.prototype` now holds `toString` and `valueOf`, sharing the Number
  prototype natives: `Meth::NumToString` already answers a `Val::BigInt`
  receiver (decimal, or the radix argument). It also gets the usual
  `constructor` back-pointer.
- engine tests **1021 -> 1022** green (new
  `bigint_prototype_methods_are_borrowable`: the object, the constructor, a
  borrowed decimal call, a borrowed base-16 call, and the own property).
- `github.com/search?q=test&type=repositories`: **no script errors**. The
  blob, pull-request, `/pulls`, `/issues/new`, repo, and issues pages all run
  clean as well.

## Subclassing `Event` (P62, 2026-11-11)

`github.com/topics/rust` and `github.com/rust-lang/rust/releases` logged:

```
custom element <relative-time>: connectedCallback threw
(Uncaught TypeError: event has no type)
```

GitHub's relative-time element dispatches an event it builds as a subclass:

```js
class P extends Event {
  constructor(oldText, newText, oldTitle, newTitle) {
    super("relative-time-updated", { bubbles: true, composed: true })
    ...
  }
}
...
this.dispatchEvent(new P(e, C, t, i))
```

Our `Event` native always allocated a fresh `Event.prototype` object and
ignored the `this` it was handed. A plain `new Event` looked fine, but from
`super(...)` the return value is discarded, so the subclass instance never got
`type`, and `dispatchEvent` rejected it.

- the native now initializes the instance it was given. A subclass instance
  already has its own prototype and keeps it; a plain `new Event` (whose fresh
  object has none) is pointed at `Event.prototype`. `dispatchEvent` then sees
  the type and the subclass's own fields.
- engine tests **1022 -> 1023** green (new
  `event_subclass_super_initializes_the_instance`: type, bubbles and the
  subclass fields before dispatch, `instanceof Event` and `instanceof Upd`
  inside the listener, and a true `dispatchEvent` result).
- both pages run with **no script errors** after the change.

## `Cache-Control` decides the disk-cache lifetime (P63, 2026-11-11)

Opening `github.com` after more than five minutes took **23.8 s**; Chromium
re-opens the same page in about a second. The engine had one fixed TTL for
every URL, so a page whose assets are `immutable` was re-downloaded in full
once the TTL lapsed:

```
Cache-Control: public, max-age=31536000, immutable   // every githubassets bundle
Cache-Control: max-age=0, private, must-revalidate   // the github.com document
```

The cache now reads the header instead of assuming:

- `max-age=N` is the entry lifetime, capped at one year (a bogus header must
  not pin a URL forever).
- `no-store`, `no-cache` and `max-age=0` answer 0: never served from disk.
  The body is still written, because a revalidation (P64) needs it to answer
  a 304.
- no `Cache-Control` at all keeps the old 300 s default.
- entry format is now `<unix-secs> <ttl>\n<content-type>\n<body>`; an entry
  from the single-number format still parses with the default TTL.

Measured on the real page (`MAZDA_TIME=1`, 68 requests / 9.34 MB cold):

| State of the cache | Net requests | Page time |
|---|---|---|
| 44 min old, fixed-TTL code | 68 | 23.8 s |
| rebuilt once, then reopened | **1** | **7.3 s** |

The single remaining request is the document itself, which is exactly what
the server asks for (`max-age=0`); 4.2 s of the 7.3 s is that download. That
is P64's target, not this change's.

- engine tests **1023 -> 1024** green (new `cache_ttl_follows_cache_control`).
- `cache_roundtrip_and_ttl` now also covers a year-long `max-age`, ttl 0 and
  the old entry format.

## Conditional revalidation (P64, 2026-11-11)

After P63 the only request a reopened `github.com` still makes is the
document itself (`max-age=0, private, must-revalidate`). A validator should
turn that 525 KB download into one round trip.

What the cache does now:

- the entry remembers the response `ETag` (`<ts> <ttl> <etag>\n<ct>\n<body>`,
  `-` = none). A stale entry is **returned**, not dropped, because its body is
  what answers a 304.
- `fetch_accept` sends `If-None-Match` when the entry has one. A 304 returns
  the stored body and restarts the entry's clock (lifetime from the 304's own
  `Cache-Control` when it carries one, else the entry's).
- a validator containing whitespace would break the header line, so it is
  dropped rather than written.

Measured against a local server that honours validators
(`max-age=0` + a stable ETag):

| Request | Wire | Time |
|---|---|---|
| first (200) | 136 093 B | 3.3 ms |
| second (`If-None-Match: "v1"`) | **0 B** | 14.6 ms |

The server logged `304 inm="v1"`, i.e. the validator really travelled.

Measured against `github.com`, a 304 is **possible but not reliable.** GitHub's
HTML ETag is not stable across responses: a bare `curl` cookie jar with the
previous response's tag got a `200` and a different tag every time. The
engine's own cookie store does better, but only sometimes — over four warm
runs (one per document fetch) exactly one returned `304`, `0 B`:

```
[net] 1421.0 ms        0 B https://github.com/ (304)
[net] 1620.4 ms   525736 B https://github.com/ (gzip)   // next run: new ETag
```

(An earlier note here claimed "no 304 is possible"; that was measured with a
fresh curl jar and was too strong. The revalidation code is what makes the
occasional 304 possible at all.)

So on a warm cache the document is still usually the full body, and its cost
is **latency, not bytes** (525 KB decompresses to ~92 KB on the wire, so
transfer is not what takes 1.4–1.6 s).

One real bug this shook out, fixed in P67: a 304 from GitHub still carries
`Content-Encoding: gzip` and no body, so the HTTP/1.1 reader fed an empty gzip
stream to the decoder and the whole page failed with `unexpected end of file`.
A 304 now answers from the cache without reading a body at all.

Supporting measurement — connection reuse, not multiplexing, is the lever:

```
curl https://github.com/ https://github.com/
  req1 appconnect 1.151 s  ttfb 1.575 s
  req2 appconnect 0.000 s  ttfb 0.394 s
```

A fresh connection pays ~1.2 s of TLS before the server even sees the
request; a warm one answers in 0.4 s. Our cold page opens 6+ connections and
pays that each time, which is what P65/P67 go after.

```
curl https://github.com/ https://github.com/
  req1 appconnect 1.151 s  ttfb 1.575 s
  req2 appconnect 0.000 s  ttfb 0.394 s
```

A fresh connection pays ~1.2 s of TLS before the server even sees the
request; a warm one answers in 0.4 s. Our cold page opens 6+ connections and
pays that each time, which is what P65/P67 go after.

- engine tests **1024 -> 1025** green (new
  `cache_keeps_the_validator_for_revalidation`).

## Overlapping the image phase with script execution — rejected (P65, 2026-11-11)

The plan was cheap on paper: the markup already names its images, so start
those downloads before the page's scripts fetch and run, and the ~2 s image
phase becomes a cache hit.

Built it: a DOM walk that resolves `<img srcset>`/`<picture>` the way layout
does, a thread that starts the image lanes before `run_scripts`, joined before
layout. Measured with a *provably* cold cache (`LOCALAPPDATA` pointed at a
fresh directory, so nothing is inherited from earlier runs):

| Config | Page time |
|---|---|
| baseline | 14.7 s, 16.8 s |
| prefetch on | 22.8 s, 22.4 s, 22.1 s, 23.0 s |
| prefetch off, same binary | 18.6 s, 25.6 s |

Two things went wrong:

1. the CSS/JS preload grew from **3.97 s to 6.33 s**. The lanes compete for
   the same ~300 KB/s; ordering bytes does not make them smaller.
2. the box tree asked for 11 image URLs while the markup walk produced a
   different set of 21, so the image phase still cost 2.1–2.8 s. GitHub's
   images are largely inserted or rewritten by script, so the pre-script DOM
   is not the image set the page ends up using.

The A/B on one binary (alternating, empty cache each run) is inside the
noise band: 22.1/23.0 s with prefetch, 18.6/25.6 s without. No measured win,
so it is reverted and recorded here rather than retried — on a bandwidth-bound
link, overlap cannot pay.

The experiment did expose a real hazard, which is kept: concurrent preload
lanes can read the cache while another lane writes it. Writes now go to a
unique temp file and are renamed into place, so a reader sees either the whole
old entry or the whole new one. Test:
`cache_reader_never_sees_a_partial_entry` (a reader racing 120 writes of a
100 KB entry never observes a torn body).

- engine tests **1025 -> 1026** green.

## `Content-Encoding: br` (P66, 2026-11-11)

Every bundle travelled in gzip while Chrome's copy was brotli. ureq has a
`brotli` feature, but it pins `brotli-decompressor` 4.0.0, which this
project's offline registry does not carry (5.0.3 is there), so the dependency
is direct and the header and the decode are ours:

- `Accept-Encoding: gzip, br`. `deflate` and `zstd` stay absent — advertising
  a format with no decoder only buys unrenderable bytes.
- `decode_br` runs after the body is read, capped at 64 MB *decoded* (the
  compressed read was already capped at 20 MB).
- media probes now ask for `identity`: `Range` counts bytes of the encoded
  body, so a compressed answer would be an undecodable fragment.

Measured over this page's own 50 compressible responses (curl, parallel, the
same two header values the engine sends):

| Header | Wire bytes |
|---|---|
| `gzip` | 2 230 680 |
| `gzip, br` | 1 950 382 |

**280 298 B saved (12.6%)**, 49 of 50 responses smaller. Examples:
`landing-pages` 762 977 → 626 747, `primer-react` 150 309 → 131 363,
`23-…` 129 513 → 117 755.

`github.com`'s document still arrives gzip — GitHub does not brotli the HTML —
so the asset path is what improves. The engine logged **56 `(br)` responses
out of 69** on a cold page, with no decode errors and the same single known
script error at the same bundle offset, i.e. the decoded bytes are the real
bundle.

- engine tests **1026 -> 1027** green (new
  `brotli_body_decodes_to_the_identity_bytes`, decoding a real 1068 B
  githubassets body kept as `tests/fixtures/rxr.js.br` into its 3819 identity
  bytes).

## HTTP/2 (`MAZDA_HTTP2=1`) (P67, 2026-11-11)

Over HTTP/1.1 every preload lane is its own TCP connection and its own TLS
handshake, and requests on one connection are serialized. The P64 note already
had the number: a fresh connection answers in **1.58 s**, a reused one in
**0.39 s** — and the cold page opens 6+ connections across its phases.

`h2net` is a hyper client over `hyper-rustls` with `h2` + `http/1.1` ALPN.
The engine stays synchronous: a two-worker tokio runtime is created on first
use and `block_on` bridges each fetch. Any error falls through to the ureq
HTTP/1.1 path, so a server that only speaks HTTP/1.1 still loads. The whole
feature is behind `MAZDA_HTTP2=1` until it has a track record.

Two implementation notes worth keeping:

- `hyper-rustls`'s builder **panics** on a `ClientConfig` whose ALPN list is
  already set ("ALPN protocols should not be pre-defined"), so `tls_config_h2`
  returns the shared base config untouched and the builder sets ALPN from the
  enabled protocols.
- hyper hands back the body **as sent**, so `finish_fetch` now decodes both
  `br` (ours) and `gzip` (ureq used to do this) behind one shared tail. A 304
  is answered from the cache before any decoding: GitHub sends
  `Content-Encoding: gzip` with an empty body on a 304, which used to fail the
  whole page with `unexpected end of file`.

Cold, provably empty cache (`LOCALAPPDATA` pointed at a fresh dir), five
alternating pairs. The first three were one binary, the last two a rebuild
with the 304 fix; the fix does not touch a cold load (no validator is stored
yet), so the pairs are comparable:

| Pair | h2 total | h1 total | h2 module preload | h1 module preload | h2 scripts | h1 scripts |
|---|---|---|---|---|---|---|
| 1 | 13.79 s | 14.67 s | 2.27 s | 2.68 s | 5.31 s | 5.47 s |
| 2 | 13.76 s | 15.16 s | 2.22 s | 3.11 s | 5.15 s | 5.77 s |
| 3 | 15.17 s | 17.58 s | 2.40 s | 3.18 s | 5.39 s | 8.16 s |
| 4 | 18.83 s | 17.34 s | 2.16 s | 2.98 s | 5.09 s | 5.94 s |
| 5 | 15.95 s | 14.60 s | 2.08 s | 3.18 s | 4.94 s | 6.08 s |
| **median** | **15.17 s** | **15.16 s** | **2.22 s** | **3.11 s** | **5.15 s** | **5.94 s** |

So be precise about what this shows: the phases h2 targets are **shorter in
every single run** (~0.9 s off module preload, ~0.8 s off scripts, because
they reuse the handshake instead of paying it again), but the *total* is a
tie — this link's variance (13.8–18.8 s for the same page) is bigger than the
effect. h2 won 3 of 5 pairs.

Warm, fresh process (HTTP cache populated, DOM snapshot removed so only the
document is fetched): h2 4.30/4.49/4.70/5.59 s vs h1 4.00/4.04/4.04/4.63 s.
h2 is **slower by ~0.45 s** here: the document fetch is one request, and h2
pays its extra setup round trip where h1 does not.

Warm, **one renderer process doing two loads** (the product's shape: the
renderer survives navigation, so the connection pool and TLS session cache
persist). Each transport started from an identical copy of the cache:

| Transport | Second load | Document fetch |
|---|---|---|
| h1 | 3.57 / 3.58 / 3.61 s (median 3.58) | 0.46–0.83 s |
| h2 | 3.21 / 3.89 / 4.27 s (median 3.89) | 0.49–1.33 s |

Still no win for the single-request case. The cold page-load phases are where
h2 helps; a reopened page does not exercise them. See P68 for the decision
this led to.

Warm breakdown (h1, one request):

```
[net]  461.9 ms        0 B https://github.com/ (304)
[time] scripts: 1649 ms (fetch 41 ms, exec 1608 ms)
[time] cascade + box tree: 775 ms
...
[nav] worker finished https://github.com/ in 4309 ms
```

That is the useful new baseline: on a warm page the network is **~0.5–1.4 s**
and the engine is **~2.8 s**, so the next lever is JavaScript execution and
style/layout, not transport.

- engine tests **1027 -> 1029** green (new
  `gzip_body_decodes_to_the_identity_bytes` and
  `a_304_answers_from_the_cache_and_ignores_the_content_coding`).

## HTTP/2 stays opt-in (P68, 2026-11-11)

The obvious next step was to make h2 the default. The measurements above say
no, and the rule is one measured change per commit, so this one is a decision
with no code:

- cold totals: **tie** (median 15.17 vs 15.16 s over five pairs);
- warm, fresh process: h1 faster by **0.45 s**;
- warm, same process: h1 faster by **~0.3 s**;
- the multi-request phases: h2 faster by **~0.9 s** (module preload) and
  **~0.8 s** (scripts) *every* run.

Reopened pages are the common case once the cache is warm (assets are
`immutable` for a year, the document is one request), and that is exactly the
case h2 loses. Shipping h2 by default would trade a measured loss on the
common path for a gain on the rare one, so `MAZDA_HTTP2=1` stays the switch.

The interesting open question is *why* the single-request case is slower:
even with a warm pool the document fetch is 0.5–1.3 s over h2 against
0.46–0.83 s over h1, which looks like the idle connection is not being reused
(h2 pays its setup round trip again). If that is fixed, h2 should win warm
too. That deserves its own spike rather than a guess.

## `Intl.Segmenter` removes the last github.com script error (P69, 2026-11-11)

Cold github.com was down to a single uncaught error:

```
[js] script error in https://github.githubassets.com/assets/landing-pages-7116aae34b5bcfcd.js:
     ... (throw in .../wp-runtime-4adecc5a339f60b0.js): Uncaught TypeError:
     undefined is not a function (at 2:78064 of .../wp-runtime-...js)
     near `Object.defineProperty(e,Symbol.toStringTag,{value:"Module"}),`
```

The reported span was stale. `call_span` is now read against `call_src`
(P69a), but even with the right source the span had been set by the last
explicit `Call` in the webpack runtime while the throw came from an
*implicit* call (a `new`) inside landing-pages' `D8E` module. Walking the AST
down to the executing statement - `cur_span`, then the quoted tokens - landed
on tabbable's module-scope feature test:

```js
let cU,cq,cW,cJ="u">typeof Intl?new Intl.Segmenter:0,...
```

`Intl` exists in this engine but `Intl.Segmenter` did not, so the bundle's own
feature test passed and `new Intl.Segmenter` threw while the module was
evaluated, aborting all of `landing-pages`.

Fix: implement `Intl.Segmenter` over `unicode-segmentation` 1.13.3 (already in
the lock file through eframe's text layout - no new download): extended
grapheme clusters by default, `word`/`sentence` when asked, `segment()`
returns the `{segment, index, input}` entries as a plain array (iterable, all
`[...s.segment(x)]` needs), and `resolvedOptions()` reports the granularity.
Shortcut: no locale data, so the locale is always `en-US`, and the result has
no `containing()`.

Measured: cold github.com script errors **1 -> 0** (the only one it had), same
frame size and title, no new `[net]`/`[js]` errors, `[mem] after layout`
319.2 MB. engine tests **1030 -> 1031**.

The other two `[badcall]`s on the page are inside `try`/`catch` blocks:
core-2d2c... probes `Intl.DateTimeFormat().resolvedOptions()` (that prototype
only has `format`), and pb-... builds a `ClipboardItem` to test clipboard
support. Both are candidates for a later measured pass, not page blockers.

## h2 flow-control windows are not the warm-load penalty (P70, 2026-11-11)

P68 left one open question: why is a warm one-request h2 load slower than h1?
The first hypothesis was h2's default 64 KiB stream/connection window: a
525 KB document would then stall about once per window waiting for a
WINDOW_UPDATE round trip. `hyper-util` exposes the knobs, so the client was
changed to `http2_adaptive_window(true)` plus a 1 MiB stream / 8 MiB
connection window and measured against the untouched client.

Method: 10 warm loads, fresh process each, alternating old and new client,
HTTP/2, with the document cache entry deleted before every run so all ten
transfers are 200s of the same 525 KB body (otherwise a lucky 304 hides the
body path). Metric is the `[net]` line for the document.

| Client | Document fetch (ms) |
|---|---|
| old windows | 1952, 2092, 1953, 4146, 4109 (median 2092) |
| new windows | 2068, 2012, 2201, 3986, 2457 (median 2202) |

Paired differences (new - old): +116, -80, +248, -160, -1652 ms. Mixed signs,
no consistent direction; the two ~4 s pairs are a network slowdown that hit
both clients. **Rejected**: the change was reverted, the client keeps the
hyper defaults.

So the warm penalty is not flow control. Two other observations from the same
session:

- 525 KB of HTML takes 1.9-4.1 s on this link today (~250 KB/s), which is
  larger than the 0.3-0.45 s h2 penalty P68 measured. Warm h1/h2 comparisons
  are dominated by whatever the network is doing, which is why P68 needed
  alternation to see anything at all.
- In a 10-run h1/h2 alternation, h1 got 2/5 304s and h2 0/5. Both paths build
  the same header list (`If-None-Match` included, app.rs), so this is the
  rotating github.com ETag, not a transport difference.

Conclusion: `MAZDA_HTTP2=1` stays opt-in (P68 unchanged). To learn anything
more about warm h2 the next step would have to count TLS handshakes/reused
connections directly rather than compare wall-clock single requests.
## Splitting a page load between the interpreter and its natives (`MAZDA_PROF=1`) (P71, 2026-11-11)

Script execution was 40-70% of a github.com load (3.8 s warm, 7.0 s cold) and
nothing said where inside it the time went. A tree-walker has no profile
stack, so the engine now times every native (host + builtin) call wall-clock
under `MAZDA_PROF=1` and dumps the top 25 after each script turn. Per-module
parse/run times are printed under `MAZDA_TIME` as well. Both are diagnostic
only: the timer is compiled in but skipped unless the variable is set.

One warm load, cache populated, DOM cache deleted:

```
[time] scripts: 3939 ms (fetch 99 ms, exec 3840 ms)
[prof] native total 1082 ms across 122776 calls
[prof]    445.8 ms       65 calls  customElements.define
[prof]    444.0 ms        4 calls  JSON.parse
[prof]     29.7 ms      875 calls  Map.prototype.set
[prof]     18.3 ms    18122 calls  Set.prototype.add
[prof]     16.9 ms       83 calls  forEach
[prof]     16.3 ms    12515 calls  Object.defineProperty
[prof]     16.1 ms       10 calls  Array.from
[prof]     12.9 ms      347 calls  map
[prof]     10.2 ms    22230 calls  Set.prototype.has
```

Reading:

- 122 776 native calls cost 1.08 s, so ~2.76 s of the 3.84 s is the
  interpreter itself (statement dispatch, identifier lookup, property
  access). That is the next target after the item below.
- `customElements.define` is *inclusive*: it upgrades matching elements, and
  the single `<react-app>` upgrade runs the page's React boot (471 ms when
  timed separately). Not overhead, and nothing to delete.
- 122 776 calls in 1.08 s is ~8.8 us per call overall; the per-call native
  cost stays in the microsecond range, so the native surface is not bloating
  loads.
- Parsing is cheap: 275 KB `react-core` parses in 71 ms, 456 KB `pb` in 87 ms,
  538 KB `k0` in 49 ms. The bundle time is in running, not parsing.

One row was worth a second look: `JSON.parse` 444 ms over 4 calls, i.e.
~110 ms for a single parse. The per-call log (added with P72) shows it is one
133 KB parse, and that parse cost 461 ms: the string reader validated the
entire remaining buffer once per character. Fixed in P72.

## JSON string parsing was quadratic per character (P72, 2026-11-11)

P71 left one native row that was pure overhead rather than page work:
`JSON.parse` at 444-478 ms per load. The per-call log shows it is a single
133 KB parse, and `Jp::string` had two problems on a string that size:

- every plain character went through `String::push(char)` after decoding
  `chars().next()`, so the string was built one code point at a time;
- worse, each of those steps called `std::str::from_utf8` on the *whole
  remaining buffer*, so one 133 KB string validated ~9 GB of bytes: O(n²).

The reader now scans to the next `"` or `\`, validates that run once, and
either returns it as-is (no escapes: no intermediate `String`) or appends it
whole to the escape buffer. `n_json` also stops copying a `Str` argument with
`to_string()`; a refcount bump replaces a 133 KB memcpy.

| github.com warm load | before | after |
|---|---|---|
| `JSON.parse` 133 KB | 461 ms | 3-4 ms |
| native total (122 776 calls) | 1271 ms | 220 ms |
| `<react-app>` upgrade | 471 ms | 6-7 ms |
| `[time] scripts` exec | 3.83 s / 3.84 s / 6.23 s | 3.23 s / 3.15 s |

The `<react-app>` row moves with the parse because the page's React boot
consumes exactly that payload: one quadratic loop cost ~0.9 s of the load,
split across two rows. The exec row is noisier than the attributed rows (this
host varies 60% run to run), so the attributed measurement is the one to
trust; the run pair above is consistent with it but not controlled.

## HTTP/2 is the default transport (P73, 2026-11-12)

P67 added an HTTP/2 client behind `MAZDA_HTTP2=1` and P68 kept it opt-in
because it lost on a *single warm document fetch*. That measurement never
exercised the case it was built for: a cold page load is 69 requests, and
every one of them paid its own round trip.

Cold-request arithmetic from the P71 logs: a 596-byte bundle took 1127 ms and
a 2.4 MB bundle took 1168 ms, so the cost is TTFB, not bandwidth. 69 requests
over 6 HTTP/1.1 lanes is ~12 sequential rounds. Chrome does not have that
problem because HTTP/2 puts every request on one connection as a stream.

Change: `http2_on()` now defaults to true (one pooled hyper connection per
origin, ALPN picks h2); `MAZDA_HTTP2=0` or `MAZDA_HTTP1=1` restores the old
path for A/B. Preload concurrency follows the transport: 6 lanes over
HTTP/1.1 (a bigger burst only queues in the accept backlog), 24 over HTTP/2,
where the limit is memory rather than sockets.

Cold github.com, fresh profile each run, alternating modes, wall clock from
process start to exit:

| Pair | HTTP/1.1 (6 lanes) | HTTP/2 (24 lanes) |
|---|---|---|
| 1 | 20 300 ms | 16 419 ms |
| 2 | 19 522 ms | 15 637 ms |
| 3 | 20 400 ms | 14 810 ms |
| 4 | 18 398 ms | 17 959 ms |
| median | 20 300 ms | 16 419 ms |

Every pair favoured HTTP/2, median **-3.9 s (-19%)**. Both modes fetched the
same 9.41 MB in the same 69 requests with zero script errors, so this is
transport only. The `[time] preload` line for the 25 initial subresources went
from 3.7-3.9 s to 2.3-2.4 s; the tail of the load did not change (`scripts`
exec 5.3 s, `cascade + box tree` 1.6 s in the same run).

Pair 4 is the honest caveat: 0.44 s apart, and one h2 preload took 3946 ms
because the link slowed for both modes that round. 19% is a transport win of
the size predicted by the round-trip arithmetic, not the whole gap: the phases
still take turns (preload 2.4 s, module preload 1.5 s, exec 6.3 s, images
1.2 s), so the network is still idle during script execution. That overlap is
the next lever, not another transport tweak.

## Progressive first paint: content before the scripts finish (P74, 2026-11-12)

After P73 the total is 14-16 s cold, and the window is blank for all of it:
the pipeline fetched the markup, ran 34 modules for ~5 s, laid out, and only
then produced a frame. No amount of shaving the total fixes how that feels -
a browser is expected to show the page while it is still loading.

Chrome paints a frame as soon as it has the markup and its render-blocking
stylesheets; scripts can mutate it afterwards and it repaints. That is a
pipeline order, not a speed trick, and it costs one extra cascade.

Change (all in `crates/engine/src/app.rs`):

- `LoadDone` grows `partial`. The worker pushes one *partial* page (same
  `PageWire`, same job id) from inside the load, then the final page as
  before; `NavGate` accepts both because both carry the current id.
- `settle_load` takes an `Option<&dyn Fn(PageWire)>` sink. When present (and
  not a snapshot restore) it lays out the post-parse DOM *before* running a
  script, sets `FontsChoice::B` for that pass (system fonts, no webfont
  decode), and hands the wire to the sink. It runs on a scratch `ImgCache`.
- `layout_page` grows `first_paint: bool`: on the early pass it skips
  `probe_images` / `probe_media` and the image preload burst, i.e. every
  round trip only the final pass needs. Boxes still size from whatever the
  cache already holds; the final pass replaces them.
- The preload split follows the frame: `fetch_and_parse` now preloads only
  `stylesheet_urls` (the first paint needs those to be render-blocking), and
  `script_urls` are preloaded from inside `settle_load` once the frame is out.
- Shell: `drain_results` adopts a partial wire via `show_partial` (page,
  scroll clamp, repaint) and leaves the navigation markers, history and
  address to the final page. `run_headless` paints it and logs
  `[time] first paint`. Restores (`restore == true`) skip the early pass:
  they replay scripts from the disk cache and are already fast.
- `MAZDA_FIRSTPAINT=0` disables the pass for A/B.

Cold github.com, fresh profile each run, alternating modes, process start to
exit, `MAZDA_TIME=1`, four pairs per session:

| session | off: median total | on: median total | on: first paint (per run) |
|---|---|---|---|
| 1 | 14 592 ms | 14 866 ms | 4 543 / 4 475 / 4 229 / 4 297 ms |
| 2 (link degraded) | 20 994 ms | 18 357 ms | 4 706 / 4 682 / 6 490 / 5 835 ms |

The three frames a user now gets are: blank -> markup+CSS at ~4.5 s -> final
page at ~14.9 s. Cost is **+274 ms** median total in session 1 (one extra
cascade: 1.4 s in the on-runs vs 1.1 s final) - inside the noise, which is
+-1.5 s pair to pair. Session 2 is the useful control: the link got twice as
slow and the totals moved by ~6 s while the first frame stayed under 6.5 s,
because it only depends on the document, the stylesheets and the cascade.

Warm (populated profile, same binary): first paint 4 102 ms, total 13 411 ms.

Where the 4.5 s floor comes from, so a later change knows what it is fighting:

| phase before the early frame | cold |
|---|---|
| document fetch (200, 514 KB gzip, always revalidated) | 1.3-1.7 s |
| stylesheet preload (20 sheets, one h2 round trip) | 1.2-2.4 s |
| css parse + cascade + box tree + text | 0.9-2.5 s |

The cascade is the part we own: `[css] 31644 rules`, `491563 candidates
tested, 19249 matched` for the early pass, same again for the final. So
200-400 ms is not reachable by reordering anything - the document alone is
1.7 s from this host - and the next real lever is that cascade, which is now
paid twice per load and is what holds the first frame at 4.5 s.

Tests **1032 -> 1033** green (new
`first_paint_shows_the_markup_before_scripts_add_to_it`: a page whose script
rewrites its heading; the sink sees the markup, the final wire sees the
script's text).

## The `color-scheme` lookup swept the sheet once per ancestor (P75, 2026-11-12)

`color-scheme` inherits, so `declared_dark` walked the parent chain until a
node declared it, and at every hop it re-tested *every* rule that declares the
property. `MAZDA_TIME` on github.com: **1 431 175 selector tests and 57 247
ancestor hops per cascade pass**, against 491 563 candidates for the whole
actual cascade - the lookup was doing three times the matcher work of the
cascade it was supporting.

Change (`css.rs`):

- the `color-scheme` rules get their own `RuleIndex` (rightmost-compound
  buckets, like `rules` and `var_rules`), so `own_scheme` tests only the
  buckets the element falls into;
- `declared_dark` memoizes per node (`2` light / `3` dark / `1` "nothing up
  the chain"): the answer is the node's own declaration, else its parent's
  cached answer, so a whole subtree costs one declaration test each. The memo
  is dropped when the pointer moves (`set_hovered`: an ancestor compound can
  carry `:hover`) and when `scope_vars` changes the root polarity.

Same load, same binary, A/B via a temporary `MAZDA_SCHEME_OLD` switch: **1 431
175 -> 1 424 selector tests, 57 247 -> 1 124 hops**, three interleaved pairs.
`cascade + box tree` did **not** move measurably (844 vs 874 ms median,
+-100 ms noise), which is the honest headline: the removed work was ~35 ms of
~740 ms, i.e. below what this machine can resolve. The counters are the proof;
the wall clock is not. It still matters for the paths that re-cascade the
whole sheet on every event (hover, scroll relayout), where the same ~35 ms was
spent per pass.

Tests **1033 -> 1034** green (new
`color_scheme_memo_answers_each_element_once_and_hover_invalidates_it`: a
child resolved before its ancestor still inherits, and an ancestor `:hover`
declaration flips the answer after the cache is dropped).

## The page's images were fetched after the scripts, not during (P76, 2026-11-12)

The comment above the script phase claimed the markup's images start
downloading before the scripts run, but the only image preload lives inside
`layout_page`, i.e. *after* the whole script phase. Cold, github.com's 21
markup images were therefore 2.5-4.3 s of round trips serialized behind
~6 s of script execution, and the probe that followed them paid another
1.6-2.5 s, because by then the URLs are new asset hashes with no cache entry.

Change (`app.rs`): `image_urls` resolves every `<img src>` in the parsed DOM
(skipping inline SVG and `data:`), and `settle_load` hands that list to a
side thread that runs the same `preload_accept(..., IMAGE_ACCEPT)` the layout
pass uses - same Accept header, so the entry the probe later asks for is the
entry that was fetched. The thread is detached: the script phase starts
immediately and the downloads overlap it. CSS `background-image` URLs still
start at box-tree time, when they are known.

Cold github.com, fresh profile each run, alternating modes, wall clock:

| pair | off (images after exec) | on (images during exec) |
|---|---|---|
| 1 | 31 639 ms | 29 169 ms |
| 2 | 23 599 ms | 22 381 ms |
| 3 | 25 491 ms | 23 265 ms |
| median | 25 491 ms | 23 265 ms |

Every pair favoured the overlap, median **-2.2 s (-8.7%)** on a link that was
running 23-32 s per load that afternoon. The stage print is the direct
evidence: `[time] image probe` after layout fell from 1 560/1 825/2 008 ms to
360/392/409 ms, and `preload (images)` from 2.5-4.3 s to 0.33-0.43 s. An
earlier session with the preload on the script phase's own thread showed the
same shape (-2.8 s median of three pairs) but paid the 1.8-2.0 s inline; the
side thread is why the current change keeps the whole overlap.

Still serialized after execution, and the next candidates: `media probe`
1.1-1.8 s (one 64 KB MP4 prefix fetch, pure TTFB) and `image decode` ~0.3 s.

Tests **1034 -> 1035** green (new
`image_urls_resolve_markup_sources_and_skip_inline_ones`).

## The `<video>` header was fetched after the scripts too (P77, 2026-11-12)

The same shape as P76, one stage later. github.com's hero clip is
`preload="metadata"`, so `probe_media` only needs the MP4 container header
(64 KB range request) to size the box - but it asked for it *after* layout,
i.e. after scripts, as a single round trip with nothing to overlap:
`[time] media probe` was 1.0-2.1 s of pure TTFB.

Change (`app.rs`): `media_urls` collects each `<video>`'s own `src` plus its
`<source>` children, resolved against the page. `settle_load` gives the first
four to a second detached thread that runs the probe's own
`media_prefix_probe`, so the header lands in `<cache>/media/<hash>.head`
before the layout-time probe asks for it. The probe keeps its range request
and its cache, so a miss (slow link, cache cleared) is exactly the old
behavior.

Cold github.com, fresh profile each run, alternating, A/B via a temporary
`MAZDA_EARLY_MEDIA` switch, three pairs:

| pair | off: media probe | on: media probe | off: wall | on: wall |
|---|---|---|---|---|
| 1 | 2 106 ms | 1 ms | 16 905 ms | 17 314 ms |
| 2 | 1 734 ms | 1 ms | 16 762 ms | 15 339 ms |
| 3 | 1 023 ms | 1 ms | 18 082 ms | 14 989 ms |
| median | 1 734 ms | 1 ms | 16 905 ms | 15 339 ms |

The stage is eliminated outright (**-1.7 s** on the probe itself); total median
**-1.57 s (-9.3%)**, two of three pairs in favour and the third +0.4 s, i.e.
inside this link's pair-to-pair spread. The `[net] ... (prefix)` line moves
into the script phase, where its round trip is free.

Tests **1035 -> 1036** green (new
`media_urls_collect_video_and_source_children`).

## One download per URL, however many threads ask (P78, 2026-11-12)

Every phase added since P74 starts its round trips earlier, which is the right
direction and also the way to pay twice: a burst preloads URLs, the sequential
phase that follows asks for the same ones before the burst finished, and both
hit the network. Nothing in the loader coordinated them - the disk cache only
helps *after* an entry is written, and an entry is written when the fetch ends.

Change (`app.rs`): `fetch_accept` now goes through `coalesce_fetch`, a small
in-flight table keyed by URL. The first caller for a URL becomes the leader
and runs the request; everyone who asks while it is running joins its answer
(a `Condvar` on a shared slot) instead of issuing a second request. The slot
is removed when the answer is published, so a later ask behaves exactly as
before (it reads the cache the leader wrote). A leader that panics publishes
an error rather than leaving waiters blocked.

Measured honestly: **no wall-clock change**, and no duplicate fetches to
remove today. Logging every `[net]` line of four cold loads (before and after)
gives 82 network fetches and **0** URLs fetched twice in each. The overlap we
have today happens to be ordered such that the disk cache always wins. So this
is a guarantee, not a saving - it is the piece that makes it safe to start a
preload *earlier* than the phase that consumes it, which is the only remaining
way to attack the first frame (see the rejected experiment below).

Tests **1036 -> 1037** green (new `coalesce_fetch_runs_one_request_per_url`:
eight threads, one request, every caller gets the bytes, and the slot is gone
afterwards).

### Rejected: warming the `<head>` with a second range request

The first frame waits on: document (1.3-2.1 s) -> stylesheet burst (1.6-2.9 s)
-> cascade (0.9 s). The stylesheet links sit at byte 3 334 of github's HTML, so
a 64 KB `Range` read of the same URL could name them ~0.5 s before the full
document lands, and the burst would start that much earlier.

Implemented and measured with `MAZDA_TIME` (fresh profile, three runs): the
prefix *is* the HTML (25 stylesheets parsed out of the 64 KB prefix, correctly
cut at the last whole tag), and it arrives at **2 319 ms** while the document
itself - 525 802 bytes of gzip - arrives at **1 736 ms**. The prefix is a fresh
round trip with no compression and it competes with the document for the same
link, so it lands *later* than the thing it was meant to beat. First paint:
4 941 ms, i.e. unchanged from the 4.2-5.3 s baseline, for one extra request.

Two bugs found on the way, worth keeping in mind for a future attempt:

- a *second* `Accept-Encoding` header is another candidate, not an override:
  the first attempt pushed `identity` next to the shared `gzip, br` and got
  brotli bytes back, which `from_utf8_lossy` turned into 118 KB of U+FFFD and
  a 64-node "document" with no stylesheets. Replace, never append.
- the cut has to be at the last whole tag, or a truncated `href=` becomes a
  request for a truncated URL.

What this rules out is *asking twice*; it does not rule out reading the one
response as it arrives (streaming decode of the gzip/br body, parse the head
from the first chunk, preload from there), which is what the next attempt
should do - and what `coalesce_fetch` above exists to make safe.

## Measure the frame, not the screenshot (P79, 2026-11-12)

Two statements were both true and neither was checkable: "first paint 4.7 s"
and "the page is blank for twenty seconds". A load publishes *several* frames -
the markup + stylesheet pass, then the post-script layout, then any relayout a
later script mutation forces - so a single number says which moment was
measured, never how the page looked in between. A screenshot says even less:
it shows whatever frame was last presented, and on this host a screen capture
of the shell returned another window at the same rectangle, so it cannot be
trusted either.

`tools/paint-timeline.ps1` drives the renderer exactly the way the shell does
(`resize`, `load`, frames announced on stdout) and samples the published frame
buffer every 200 ms, printing a row whenever the sequence number changes:

```
Time Frame  Ink Colours Note
 4.1     1 18.85     223 new frame
```

Both numbers come from the renderer's own pixels: `Ink %` is the share of
sampled pixels that differ from the dominant colour, `Colours` the count of
distinct RGB values in the sample. A blank window reads 0 % / 1 colour, a
text-only first paint a couple of percent, the finished page two digits. `Ink`
alone is easy to mistake (one large flat block scores high), so the pair is the
reading: **26.4 % ink in 5 colours** is a page of solid rectangles, **18.9 % in
223 colours** is text with antialiasing. Every 4th row and every 4th pixel is
sampled, so a sample costs a sixteenth of a frame and does not itself slow the
load down. `-PngDir` writes each new frame as a PNG (the renderer's pixels, not
a capture) when the frame needs looking at.

The tool's first use is the three render rules that follow: cold github.com,
fresh profile, `-Seconds 30 -Every 5` went from `4.7 s 26.42 % 5 colours` (the
flat-block render) to `4.1 s 18.85 % 223 colours` (hero text, logo, CTAs), with
the finished frame at 19.18 % / 281 colours. `example.com` reads 148 colours and
the local test pages 124-235, which is what a text page looks like.

## `cancelAnimationFrame` actually cancels (P80, 2026-11-12)

`requestAnimationFrame` has always returned an id; nothing consumed it. The id
was a counter with a comment saying so, and `cancelAnimationFrame` was simply
not a global - so a page that called it threw `ReferenceError` and the effect
that made the call died with the turn. github.com's `landing-pages` bundle
calls the pair in four places: its hero canvas cancels the frame it queued
from a `ResizeObserver` callback, and GSAP's ticker cancels the previous
tick's frame every tick.

Change: the id now travels with the callback. `Ctx.raf` and
`PageScripts.raf` hold `(id, callback)`, `cancelAnimationFrame(id)` records
the id in the engine (`JsEngine::cancelled_frames`, the same shape as
`cancelled_timers`), and both harvest points - `harvest_raf_from` when a turn
ends and `run_due_frames` before a pass picks its due batch - drop the named
frames first. A cancel therefore works from the same turn, from a later timer
turn, and for a callback already sitting in the queue.

Measured: no pixels of its own - github's hero reveal it unblocks is the
subject of the next two rules. What changes is the console: the
`ReferenceError: cancelAnimationFrame is not defined` line is gone and the
effects that used to abort now reach their next statement. Tests
**1037 -> 1039** green (`cancel_animation_frame_drops_queued_callback`,
`cancel_animation_frame_from_a_timer_drops_queued_frame`).

## Report reduced motion, because this build does not animate (P81, 2026-11-12)

github.com's homepage decides how to reveal its hero from
`window.matchMedia("(prefers-reduced-motion)").matches`:

```js
z = W || er ? {} : { opacity: 0 }   // W = prefersReducedMotion
```

`W` true means "no intro animation, render the content as it is"; false means
"start the hero at `opacity:0` and let the GSAP timeline bring it in". This
engine has no transitions and no Web Animations: it paints a frame and moves
on. So reporting `no-preference` was a promise it cannot keep - the hero got
its `opacity:0` start state and the timeline that was supposed to lift it
waits on `document.fonts.ready` and a vsync that never arrive.

`css.rs` now has `PREFERS_REDUCED_MOTION = true`, used by `media_feature` for
`@media (prefers-reduced-motion: reduce)` (and the boolean form), mirrored on
the shared host (`DomHost::match_media`, renamed from `match_media_dark`) so
`matchMedia` in JS and the cascade always agree. A page that respects the
preference paints its end state immediately instead of waiting for a tick.

Measured together with P82 below (they are the two halves of the same reveal,
and separating them would have meant a build that paints a hero nobody can
see): cold github.com, fresh profile, `tools/paint-timeline.ps1`: the flat
block render (`26.42 %` ink in `5` colours) becomes `5.00 %` ink in `92`
colours - real text, in a page that is now `14 645 px` tall because the hero
grid lays out at full width (P83 fixes the width itself). Tests stay at
**1039**; `media_queries_evaluated_against_viewport` and the `matchMedia` test
grew the reduce / boolean / no-preference assertions.

## An inline `opacity:0` is a start state, not a hide (P82, 2026-11-12)

`opacity: 0` used to mean "invisible, skip the subtree" (`display_none`), which
is right for the common drawer/modal pattern - hidden until a class flips it -
and wrong for the other common pattern: a page writing its fade-in *start
state* into the markup, `style="opacity:0"`, and expecting a tween to lift it.
This engine runs no tweens, so the element was never shown at all.

On github.com that element is the hero's entire content column
(`div.Intro__content__klpGdB style="opacity:0"`), `<h1>Software still starts
with you</h1>` included. The `MAZDA_DUMP_BOXES=2` walk said it plainly:
`Intro__content` `h=0`, `Intro__grid_content` `h=0`, and no `h1` box anywhere.
The CSS for it is `display:flex` with `container:homepage-signup/inline-size` -
nothing hides it.

The rule now splits by origin: an **inline** `opacity:0` is an animation start
state and is painted at its end state (`opacity = 1.0`), because painting the
start state would mean painting nothing; an **inline** `display:none` still
wins, and a stylesheet `opacity:0` still hides (the drawer/modal shortcut is
kept). `resolve_with_parent` tracks both while it applies the inline block and
only undoes the hide when no `display:none` came from either side.

Measured with P81 (same reveal): `26.42 %` ink in `5` colours -> `5.00 %` in
`92`, i.e. text where there was a flat block; the frame shows the banner, the
centred headline, the CTA row and the octocat. The page was still `14 645 px`
tall and the headline wrapped to one character per line - that is the grid bug
in P83, not this rule. Tests stay **1039**; `opacity_zero_hides_subtree` grew
the inline-start-state and inline/stylesheet `display:none` cases.

Diagnostic added with it: `MAZDA_JS_STYLE=1` prints
`[style] node N <prop>: <value>` for script writes of `opacity`, `display` and
`visibility`, so "why did this element disappear" can be answered from the log
instead of a screenshot. On github it reports exactly one write, a
`display: none` on the cookie banner.

## `grid-column: span N` covers N tracks (P83, 2026-11-12)

With the hero painted (P81/P82) it was still unreadable: the headline wrapped
one character per line and the whole page was 14 645 px tall. The box dump
named the container: github's homepage grid is twelve `minmax(0,1fr)` tracks
(`Primer_Brand__Grid-module__Grid___H4Yx1`), every content column inside it
says `grid-column: span 12`, and the column came out 72 px wide.

Cause: the parser took `span 12` for a *line name* - `grid_col = Some("span
12")` - which never matches a `col_lines` entry, so placement fell through to
auto and the item took a single track. `grid-column-end: span N` was dropped
outright by a `starts_with("span")` guard.

Change: `BoxStyle::grid_col_span` (0 = one track) filled from either half of
the `grid-column` shorthand and from `grid-column-end`, via one small
`span_count()` helper; `grid()` placement wraps the item to the next row when
`span` no longer fits the explicit template and occupies the covered tracks.
Named lines still win when the value is a name, so the `content` /
`extended-full-end` bleeding (MDN's header) is unchanged.

Measured (all four rules in the build): cold github.com, fresh profile,
`tools/paint-timeline.ps1 -Seconds 30 -Every 5`:

| | first frame | finished frame | stage lines |
|---|---|---|---|
| before (flat blocks) | 4.7 s, 26.42 % ink, 5 colours | 19.3 s, 55.74 %, 10 colours | first paint 4 907 ms, scripts 5 067 ms |
| after | 4.1 s, 18.85 % ink, 223 colours | 19.5 s, 19.18 %, 281 colours | first paint 4 295 ms, scripts 4 627 ms |

The number that matters is the colour count: 5 colours is a page of solid
rectangles, 223 is text with antialiasing - the banner, the centred "Software
still starts / with you", the CTA row, the logo and the nav icons are all in
the first frame. The ink percentage *fell* because a correct layout is mostly
white page; it is read together with `Colours`, never alone (P79). Frames
3 at 4.09 / 11.5 / 19.5 s (markup+CSS, post-script, relayout). Tests
**1039 -> 1041** green (`grid_column_span_parses_from_shorthand_and_longhand`,
`grid_column_span_covers_its_tracks`).

## A theme is chosen by what matches `<html>`, not by a token in a class (P84, 2026-11-12)

With the layout right, github.com was still a mixed page: white body, dark
bands. At x=500 the frame read `#010409` for y 0-11, y 615-669, and a dark
strip down the right edge (`#000308` at x=990), while the body read white.
The banner, the headline and the CTAs were painted light - the *canvas* was
dark. That is the html element's background surviving from a palette the page
never selected.

github ships `<html lang="en" data-color-mode="light" data-light-theme="light"
data-dark-theme="dark">` - a data ATTRIBUTE, not a theme class - and Primer
Brand then declares one palette three times:

```css
:root, [data-color-mode=light], [data-color-mode=auto] { --brand-color-canvas-default: white }
[data-color-mode=dark]                                  { --brand-color-canvas-default: black }
@media (prefers-color-scheme: dark) { [data-color-mode=auto] { ... black } }
```

`scope_vars` picked the root's polarity with `theme_polarity(class)`. There is
no class on that `<html>`, so `root_pol` was `None`, no definition could be
marked as contradicting the root, and the fallback branch ("last definition
wins") took the **dark** sheet - `#010409` - under a page that says light. The
brand tokens in the same sheet are `:root`/`[data-color-mode=light]`, so the
rest of the page stayed light: exactly the mixed render in the frame.

Two changes, one rule:

* `root_pol` reads `data-color-mode` / `data-theme` first, then `class` - the
  theme can be declared as an attribute, and github declares it that way.
* a definition whose selector **matches the real `<html>`** counts as on-root
  regardless of `root_pol`: matching the element that owns the custom
  properties is the truth, the polarity tokens are a heuristic. (This is why
  `root_pol.is_some() &&` had to go from the `applies` guard - an unpolarised
  root still has a real root.)

Reproduced locally before fixing, with the smallest page that has the same
shape (`%TEMP%\mazda-local\theme.html`: `data-color-mode="light"` plus the dark
sheet last) - it rendered the dark palette. After: white canvas, black text,
`2.22 %` ink in `68` colours.

Cold github.com, fresh profile, `tools/paint-timeline.ps1 -Seconds 50 -Every 5`
(this run's document fetch alone was 18.2 s - the link was bad, so only the
pixels are comparable, not the timestamps):

| | canvas pixels (`#010409`-ish) | white pixels |
|---|---|---|
| before | 14.9 % | 80.9 % |
| after | 2.9 % | 92.0 % |

No dark band at the top or the bottom, no dark strip at the right edge; the
first frame at 38.1 s reads `7.54 %` ink in `220` colours. The remaining 2.9 %
is real dark content (the secondary CTA painted opaque black - next rule).
Tests **1041 -> 1042** green
(`scope_vars_picks_the_palette_whose_selector_matches_the_root`).

## A background colour's alpha is a blend, not a suggestion (P85, 2026-11-12)

The first frame was light and readable, and one element was still wrong: the
secondary CTA ("Download GitHub Copilot app") painted as a solid black bar with
its black label invisible on it. Its background is
`--brand-button-secondary-bgColor-rest: #0000000f` - 6 % black, i.e. a light
grey pill. `parse_color` dropped the alpha byte (`8 => Some(digits >> 8) //
drop alpha, lenient`), so the pill was painted `#000000` and the label
disappeared into it. The 4 px bands at y 322-325 and 412-415 had the same
cause: 6 %-alpha fills painted as `#000000`.

Change: `BoxStyle::bg_alpha` (255 = opaque) keeps the alpha beside the RGB
value, so every existing reader of `bg` still sees a plain colour; `apply_decl`
reads `#RRGGBBAA` / `rgba()` / `hsla()` through the already-existing
`parse_color_alpha`, and layout folds the alpha into the rect's packed opacity
(`bg_alpha_of`, multiplied by the box's `opacity`). `alpha == 0` stays "no
fill", so `background: transparent` still leaves an earlier declaration in
place.

That exposed a second, older shortcut: a rounded border was painted by filling
the whole border box with the border colour and laying the background on top,
which is invisible while the background is opaque but shows the border colour
through a translucent one - the pill came out `#C5CCC7` (6 % black over
`#D2D9D4`) instead of `#F0F0F0` (6 % black over the page). So a rounded border
now paints as a **ring**: `Layout::rect_ring` is a channel parallel to
`rects` (`Some(t)` = "paint the band inside the outer radius"), the ring is
pushed *after* its background, and `paint_ring_rect` paints two spans per row
(outer span minus inner span) - no per-pixel distance test, no work in the
interior. Opaque pages are byte-identical: the same border colour ends up on
the same pixels.

Measured, cold github.com, fresh profile, `tools/paint-timeline.ps1`:

| | secondary CTA area | 4 px band at y=322 | first frame |
|---|---|---|---|
| before | 83.9 % solid `#000000`, label invisible | `#000000` | 7.0 s, 7.54 % ink, 261 colours |
| after | `#F0F0F0` interior, `#D2D9D4` 1 px ring, label readable | `#F0F0F0` | 8.4 s, 7.55 % ink, 237 colours |

(The timings are two different runs on a noisy link; only the pixels are
comparable.) Local check page (`%TEMP%\mazda-local\alpha.html`) reads
`#F0F0F0` for `#0000000f`, `#C3C3C3` for `#0000003c`, `#7F7F7F` for
`rgba(0,0,0,.5)` and `#000000` for opaque black. Tests **1042 -> 1045** green
(`translucent_backgrounds_keep_their_alpha`,
`translucent_backgrounds_blend_instead_of_covering`,
`a_border_ring_leaves_the_translucent_fill_visible`).

## The input hint has its own style, and `:placeholder-shown` is static (P86, 2026-11-12)

The sign-up field on github's front page painted its label and its hint on top
of each other - "Enter youemail". Two separate causes, both fixed here.

First, the hint was unconditional: `<input placeholder="you@domain.com">` drew
that text whenever the `value` attribute was missing, ignoring
`.SignUpForm__input__JlFC3S::placeholder{opacity:0}`, which is how a
floating-label field hides its hint at rest. `::placeholder` is now a real
pseudo-element bucket (`PseudoEl::Placeholder`, `Sheet::pseudo[3]`), the
parser keeps it instead of dropping the selector, and layout resolves it while
building the control: `opacity: 0` builds no hint text at all, otherwise the
pseudo's `color` and `opacity` go on that text run only. An empty `value=""`
now also shows the hint (it fills an empty control in Chrome too).

Second - the actual overlap - the label was *scaled*. Primer floats the label
with
`.SignUpForm__field:has(.SignUpForm__input:not(:placeholder-shown)) .SignUpForm__label{transform:translateY(calc(-50% - 8px)) scale(.8)}`.
`:placeholder-shown` was on the "dynamic, can never hold" list, so
`:not(:placeholder-shown)` was *vacuously true* and `:has()` matched a field
whose hint is visibly showing. The rule then applied `scale(.8)`, and the
documented transform shortcut only maps a run's anchor, not its glyphs, so the
three words moved 3 px closer to each other while their ink stayed full size -
exactly the overlap. `:placeholder-shown` is statically decidable (a text
control with a `placeholder` attribute and an empty value), so it is now a real
`Pseudo::PlaceholderShown`; `:not()` keeps it as a member instead of dropping
it.

Measured on the sign-up field (local repro with github's own CSS,
`%TEMP%\mazda-local\ghform.html`), before -> after:

| | label runs | hint | label scale |
|---|---|---|---|
| before | x = 21, 51, 77 with widths 33, 29, 34 (gaps -3 px) | hidden | .8 |
| after | x = 21, 58, 91 with widths 33, 29, 34 (gaps +4, +3.6) | hidden | 1 |

Cold github.com, fresh profile, `tools/paint-timeline.ps1`: first frame 6.4 s,
7.82 % ink, 229 colours (previous run 7.2 s / 7.78 % / 231); the field now
reads "Enter your email". The 8 px float offset and the scaled glyphs stay
unimplemented - a genuinely scaled run still overlaps, because text scaling
needs an affine glyph raster path this engine doesn't have (documented at
`transform_runs`). Tests **1045 -> 1048** green.

## The first frame can paint an inline `<svg>` (P87, 2026-11-12)

github.com at 1000 px serves its *compact* header: the logo, the person icon
and the hamburger are inline `<svg>` markup, not files. The early frame ran no
image work at all - it skipped every `<img>` *and* every `<svg>` - so the
header painted as three empty slots and only got its icons after the second
pass, ~7 s later on the same visit.

An inline `<svg>` needs no request: its markup is already in the document, so
`probe_images_local` walks the same tree on the first pass and probes only
`src` starting with `<`; a real URL stays `Pending` for the final pass, where
it is fetched as before (`first_frame_probes_inline_svg_but_not_network_images`
asserts one `probed 16x16` beside one `pending a.png`).

Measured with `tools/paint-timeline.ps1` on cold github.com, fresh profile:

| | first frame | header icons | `image probe` |
|---|---|---|---|
| before | 7.82 % ink, 229 colours | absent | 0 ms |
| after | 7.98 % ink, 235 colours | logo, person, hamburger | 0 ms |

A local page (`%TEMP%\mazda-local\svgfirst.html`) confirms the inline icon is
rasterized in the very first frame, not the second. Tests **1048 -> 1049**
green.

Measurement note: the earlier run also reported a `0.0 s, 55.74 % ink,
10 colours` frame - a *dark* page with a white header. That was the previous
renderer's last frame: the frame file is named by pid, the OS reused the pid,
and the checker sampled the leftover before this process repainted. The first
real frame is the `7.98 %` one. `paint-timeline.ps1` now deletes the file
before it starts the renderer, and a rerun reports `no frame yet` until the
real paint instead of a stale image.

## `color: inherit` takes the parent's colour (P88, 2026-11-12)

With the icons on screen the logo painted in link blue. github writes
`.logo__bX9RKh{color:inherit}`, and `inherit` was silently dropped: the child's
`Style` already held the UA `<a>` colour (`#6AB3FF`) before the author CSS
ran, so "keep what you have" looked the same as "copy the parent".

The cascade can't read the parent off that `Style` - that is the UA default, not
the parent's computed colour - so it is threaded in explicitly
(`apply_decl(..., parent_color, ...)`, `resolve_scoped(..., parent_color, ...)`,
fed from `layout`'s `style.color`). `inherit` and `unset` now copy it; `initial`
is CanvasText (`#000000`).

Sampled the logo box in the 1000x670 first frame (x 466..506, y 8..38): the
octocat's fill is now `#000000` (50 px, dominant) beside the LogoSlot's
`#e4ebe6` border, where it used to be the link blue. It is black rather than
github's `#1f2328` because the header's own `color` comes through a `var()`
chain that still lands on the initial value - smaller, separate gap. Tests
**1049 -> 1050** green (`color_inherit_reinstates_the_parent_colour`).

## The HTTP/2 path follows redirects (P89, 2026-11-12)

Typing `google.com` showed a page that said "301 Moved". The document was
fetched, so it was not a transport failure: `https://google.com/` answers `301`
to `https://www.google.com/`, and the two transports disagreed about who
follows it. `ureq` (the HTTP/1.1 path) follows redirects itself; hyper does
not, so the HTTP/2 path - the default - handed the redirect body to the HTML
parser and painted the server's own "301 Moved" page.

`h2net::get` now follows the chain: 301/302/303/307/308 with a `Location`, up to
ten hops (Chrome's limit is 20; ten is past any real chain and still bounds a
loop). A relative `Location` resolves against the URL that answered it, the
fragment is dropped, and each hop rebuilds the headers that belong to a URL:
`Cookie` comes from the jar for that hop's host/path, `If-None-Match` is
dropped (a conditional tag belongs to the URL it was stored for). A `Set-Cookie`
on a hop is stored for that hop - the hop response is dropped, so nothing else
can - which removes the "hop cookies are lost" shortcut for the HTTP/2 path.

Measured on this host, `MAZDA_TIME=1`: before, `google.com` rendered the
redirect body; after, the document is the real page (224 899 bytes, 528 nodes,
its `/xjs/` stylesheet and script follow). Tests **1050 -> 1053** green
(`next_hop_follows_the_redirect_and_resolves_it`,
`next_hop_stops_on_anything_that_is_not_a_usable_redirect`,
`a_hop_drops_the_previous_urls_cookie_and_etag`).

Still open: the page's identity (address bar, tab url, relative `src`/`href`
base) stays the URL that was typed. A redirect to a different origin therefore
resolves relative subresources against the wrong base, and the address bar
keeps showing the old URL. Fixing that means separating a page's identity from
its base URL (`KeptPage.url` vs a new `base`), which is a follow-up.

## The cascade walks 4.1 M ancestors on github's front page (P90, 2026-11-12)

The `cascade + box tree` stage is the second largest of a cold load, and the
question was whether that time is the tree walk. It is not the *element* walk -
github's front page is 2663 nodes, a few microseconds - but it is a walk:
matching one rule against one element steps up the ancestor chain compound by
compound. A new counter (`MAZDA_TIME=1`, printed next to the candidate count)
says how much:

| | count |
|---|---|
| candidate selector tests | 608 340 |
| **ancestor hops inside those tests** | **4 142 084** |
| hops per candidate | 6.8 |
| stage | 1412-3006 ms (host load varies) |

So ~7 ancestor steps per candidate is where the time goes, and the fix is not
a faster walk: it is not walking. Blink keeps a bloom filter of the element's
ancestor classes/ids/tags as it descends the tree and rejects a rule whose
left-hand compound requires a name that filter does not contain - one bit test
instead of an ancestor chain. That is the next change; this counter is the
before-number for it (4.1 M hops) and it stays, gated on `MAZDA_TIME`.





