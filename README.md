# mazda-browser

A real browser with memory as a first-class feature: a trusted shell process
plus a sandboxed renderer process. The engine — HTML, CSS, JavaScript, DOM,
layout, and paint — is written from scratch in Rust.

Phase S proved the process split is worth its cost (see `SPIKE.md`). **P1 is
in place**: a real IPC protocol, double-buffered shared-memory frames, a
renderer supervisor, tabs, snapshot restore, and a memory governor. **P2 is in
place too**: the renderer runs in an OS sandbox (AppContainer + Job Object on
Windows, `no_new_privs` + seccomp + `RLIMIT_AS` on Linux). **P3 is in place**:
`<video>`/`<audio>` play through Media Foundation + WASAPI, decoded by a
pure-Rust media crate, with DRM refused before any decoder is created. **P4 has
started**: `<a download>` streams to disk with a progress panel and lands in the
user's Downloads folder, `<input type=file>` posts files as
`multipart/form-data` through the native picker, Ctrl+F finds text in the page
with scroll-to-match highlights, and Ctrl+= / Ctrl+- zoom the page.

## Workspace

```
crates/engine     HTML/CSS/JS/DOM/layout/paint (copied from mazda-browser-rust)
crates/ipc        P1 protocol: stdio lines + double-buffered mmap frames
crates/media      P3 playback: MP4 header parse, Media Foundation, WASAPI
crates/media-probe media self-test (plain + sandboxed decode/audio/DRM)
crates/renderer   headless renderer process
crates/sandbox    OS containment for the renderer + `sandbox-probe` self-test
crates/shell      trusted shell (eframe/egui), owns the window and input
tools/            memory, reload-soak, tab, sandbox and governor harnesses
```

The engine still carries `engine::app` — the old single-process shell and
paint pipeline — because `run_headless` reuses that pipeline unchanged.

## Build

```powershell
cargo build --release --workspace
```

If the crate mirror is unreachable (the environment uses `rsproxy`), add
`--offline`; the dependency cache is already populated.

## Run

```powershell
.\target\release\shell.exe https://example.com
.\target\release\shell.exe --tabs "https://www.google.com,https://github.com" --active 1
```

The shell has no OS titlebar: the titlebar, tab strip, address bar and window
buttons are drawn in egui. Tabs are Chrome-like (hover, close, middle-click,
drag-to-move, `+`), and the window controls minimize / maximize / close.

Keyboard: `Ctrl+T` new tab, `Ctrl+W` close, `Ctrl+R` reload, `Ctrl+L` focus
address, `Ctrl+Tab` / `Ctrl+Shift+Tab` cycle, `Ctrl+1…9` select.

The shell (trusted, parent) spawns `renderer.exe` inside the OS sandbox,
forwards scroll/click over stdin, and blits the memory-mapped frame the
renderer produces. Background tabs are metadata only (URL, label, scroll), so
the single renderer holds just the active page; switching tabs restores from
the tab's snapshot.

Sandbox behaviour (P2):

- **Windows**: AppContainer with only `internetClient`; a Job Object caps
  committed memory (`MAZDA_RENDERER_MB`, default 512), allows one process,
  blocks UI/clipboard/desktop access, and kills the renderer with the shell.
  `%TMP%`/`%LOCALAPPDATA%` point at a directory granted to the package SID, so
  the frame file and cache work while the real user profile stays unreadable.
- **Linux**: `no_new_privs`, `RLIMIT_AS`, and a seccomp filter that refuses
  `execve`/`execveat`. Landlock/net-namespace filesystem isolation is the next
  increment; the code is compile-checked but not yet run on Linux here.
- `MAZDA_SANDBOX=off` is a debugging escape hatch (local test servers,
  profilers); the normal path never uses it.

Media (P3): `<video>` is a replaced element — layout sizes it from
`width`/`height`, the worker fetches the file, parses the MP4 header (DRM and
unsupported containers are refused before a decoder exists), and the renderer
decodes through Media Foundation into the page's frame. Audio goes to WASAPI
shared mode. Only `autoplay muted` starts by itself; any other video opens on
the first click, and a click inside the video toggles play/pause. The whole
playback path is in `crates/media`; on Linux it is a stub that reports
`Unsupported`. `MAZDA_NO_MEDIA=1` disables probing for an A/B memory check.

Downloads (P4): `<a href download>` saves instead of navigating. The renderer
streams the response in 64 KB chunks into its own temp dir (progress every
150 ms, no body in RAM); the trusted shell then moves the finished file into
`%USERPROFILE%\Downloads` (`name (n)` dedupe) because the sandbox keeps the
profile unwritable. Names from the `download` attribute are sanitized so a page
cannot pick a path. The toolbar's download button opens a panel with live
progress bars and clickable saved files; the status bar shows the active
transfer.

Uploads (P4): `<input type=file>` shows "Choose File", then the picked name. The
renderer only emits `pick-file <node>`; the trusted shell opens the Win32 Open
dialog, copies the file into the renderer's sandbox temp dir and answers
`file-chosen`. A POST with a pick (or `enctype="multipart/form-data"`) is built
as a boundary'd multipart body with `Content-Length`, text fields first, then
the file. Scripts never see the path or the bytes.

Find (P4): Ctrl+F opens a bar; typing sends `find <query>` and the renderer scans
the laid-out text runs itself, replying `find-result <active> <total>`. Enter
(or Shift+Enter) sends `find-step ±1` and the reply includes `scroll-to <y>`,
which the shell adopts for the active tab. Highlights are blended over the
finished frame in the renderer (yellow for matches, orange for the active one),
so closing the bar is just another repaint.

Zoom (P4): Ctrl+= / Ctrl+- step 25%…500% (Ctrl+0 resets), shown in the status
bar. The renderer lays the page out at `physical / zoom` CSS pixels and scales
the painted frame back up, so the page reflows like a narrower window; clicks
and wheel deltas are converted at the boundary.

Self-test (decode, audio endpoint and DRM rejection, plain and through the real
sandbox):

```powershell
cargo run --release -p media-probe -- --file "$env:TEMP\mazda-test.mp4"
```

Single-process reference (the pre-split app):

```powershell
.\target\release\mazda.exe https://example.com
```

## Measure

```powershell
.\tools\mem-harness.ps1 -Url https://example.com
.\tools\measure-sites.ps1            # Google, GitHub, YouTube: old vs split + scroll latency
.\tools\measure-tabs.ps1             # 10 tabs, one renderer, <= 300 MB (P1 gate)
.\tools\reload-soak.ps1 -Cycles 100  # 100 reloads, RSS plateau / live-heap leak gate
.\tools\governor-check.ps1           # tiny budget -> reclaim + snapshot restore
.\tools\crash-check.ps1              # kill the renderer -> supervisor restarts + restores
.\tools\sandbox-check.ps1            # home unreadable, no child, network, memory cap (P2 gate)
.\tools\screenshot-shell.ps1 -Url https://example.com   # capture the real window
```

Reference sites: **Google, GitHub, YouTube** — heavy, real-world pages used
for memory and latency work; `example.com` / Wikipedia stay as the light
sanity set.

Headless one-frame render (no window; useful for CI and profiling):

```powershell
.\target\release\renderer.exe --once https://example.com
```

## License

MIT — see `LICENSE`.
