# mazda-browser

A real browser with memory as a first-class feature: a trusted shell process
plus a sandboxed renderer process. The engine — HTML, CSS, JavaScript, DOM,
layout, and paint — is written from scratch in Rust.

Phase S (the current milestone) measures whether the process split is worth
its cost. See `SPIKE.md` for the numbers and the decision.

## Workspace

```
crates/engine     HTML/CSS/JS/DOM/layout/paint (copied from mazda-browser-rust)
crates/ipc        Phase S protocol: stdio lines + memory-mapped frames
crates/renderer   headless renderer process
crates/shell      trusted shell (eframe/egui), owns the window and input
tools/            memory harness, visual diff
```

The engine still carries `engine::app` — the old single-process shell and
paint pipeline — until the spike says the split is worth building further.
`run_headless` reuses that pipeline unchanged; that is deliberate.

## Build

```powershell
cargo build --release --workspace
```

If the crate mirror is unreachable (the environment uses `rsproxy`), add
`--offline`; the dependency cache is already populated.

## Run

```powershell
.\target\release\shell.exe https://example.com
```

The shell spawns `renderer.exe`, forwards scroll/click over stdin, and blits
the memory-mapped frame the renderer produces.

Single-process reference (the pre-split app):

```powershell
.\target\release\mazda.exe https://example.com
```

## Measure

```powershell
.\tools\mem-harness.ps1 -Url https://example.com
```

Headless one-frame render (no window; useful for CI and profiling):

```powershell
.\target\release\renderer.exe --once https://example.com
```

## License

MIT — see `LICENSE`.
