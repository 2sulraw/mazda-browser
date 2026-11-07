# mazda-browser

A real browser with memory as a first-class feature: a trusted shell process
plus a sandboxed renderer process. The engine — HTML, CSS, JavaScript, DOM,
layout, and paint — is written from scratch in Rust.

Phase S proved the process split is worth its cost (see `SPIKE.md`). **P1 is
now in place**: a real IPC protocol, double-buffered shared-memory frames, a
renderer supervisor, tabs, snapshot restore, and a memory governor.

## Workspace

```
crates/engine     HTML/CSS/JS/DOM/layout/paint (copied from mazda-browser-rust)
crates/ipc        P1 protocol: stdio lines + double-buffered mmap frames
crates/renderer   headless renderer process
crates/shell      trusted shell (eframe/egui), owns the window and input
tools/            memory, reload-soak, tab and governor harnesses
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

The shell (trusted, parent) spawns `renderer.exe`, forwards scroll/click over
stdin, and blits the memory-mapped frame the renderer produces. Background
tabs are metadata only (URL, label, scroll), so the single renderer holds just
the active page; switching tabs restores from the tab's snapshot.

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
