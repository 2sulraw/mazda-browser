# mazda-browser

A real browser with memory as a first-class feature: a trusted shell process
plus a sandboxed renderer process. The engine — HTML, CSS, JavaScript, DOM,
layout, and paint — is written from scratch in Rust.

Phase S proved the process split is worth its cost (see `SPIKE.md`). **P1 is
in place**: a real IPC protocol, double-buffered shared-memory frames, a
renderer supervisor, tabs, snapshot restore, and a memory governor. **P2 is in
place too**: the renderer runs in an OS sandbox (AppContainer + Job Object on
Windows, `no_new_privs` + seccomp + `RLIMIT_AS` on Linux).

## Workspace

```
crates/engine     HTML/CSS/JS/DOM/layout/paint (copied from mazda-browser-rust)
crates/ipc        P1 protocol: stdio lines + double-buffered mmap frames
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
