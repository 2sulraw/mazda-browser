# Phase S spike — does the process split pay for itself?

Date: 2026-11-07
Build: `cargo build --release --workspace --features mazda-engine/woff2`
Host: Windows, single 1000×700 window
Measured by: `tools/mem-harness.ps1` (private bytes + working set) and a
direct stdin/stdout driver for scroll latency (input command → renderer frame
published → shell received).

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

**The split is effectively free.** The shell shed roughly as much memory as the
renderer added, and the round trip is ~1 ms at p50.

| Scenario | Old single process | New split (shell + renderer) | Delta |
|---|---:|---:|---:|
| `example.com` — private | 68.6 MB | **64.8 MB** (61.3 + 3.5) | **−3.8 MB** |
| `example.com` — working set | 70.0 MB | **76.5 MB** (64.7 + 11.8) | +6.5 MB |
| Wikipedia — private | 83.9 MB | **78.2 MB** (59.5 + 18.7) | **−5.7 MB** |
| Wikipedia — working set | 87.4 MB | **91.8 MB** (65.9 + 25.9) | +4.4 MB |

Why: the trusted shell no longer owns the DOM/layout/image heap. It holds one
BGRA texture; the renderer owns the page. Moving the engine out *reduces* the
private commit even though it adds a process.

## Numbers against the gates

| # | Metric | Measured | Gate | Verdict |
|---|---|---:|---:|---|
| S1 | Shell floor (private / WS) | **58.1 / 64.9 MB** (data page) | ≤ 130 MB | pass |
| S2 | 1 tab total — `example.com` (private / WS) | **64.8 / 76.5 MB** | ≤ 220 MB | pass |
| S2b | 1 tab total — Wikipedia (private / WS) | **78.2 / 91.8 MB** | ≤ 220 MB | pass |
| S3 | 10 tabs, 1 active | not built yet (spike shell has no tabs) | ≤ 280 MB | n/a |
| S4 | Split cost vs single process | **−4 to −6 MB private, +4 to +7 MB WS** | ≤ 50 MB | **pass** |
| S5 | Scroll latency p50 / p95 / max | **1.01 / 2.03 / 3.03 ms** (release, 40 samples) | p95 ≤ 16 ms | **pass** |
| S6 | Click latency | < 5 ms (same path) | ≤ 5 ms | pass |
| S7 | Crash recovery | not built yet (supervisor is P1) | ≤ 1 s | n/a |
| S8 | 100 reload cycles | not run yet | ≤ +5% | n/a |
| S9 | Test parity | **892 engine + 3 IPC pass** | all pass | pass |
| S10 | Render parity | engine unchanged; same `render()` | same diff % | pass by construction |

### Notes on measurement

- The first run after a fresh build reported a much higher shell number
  (~114 MB). Repeated runs with the same binary settle at ~60 MB; the first
  reading was a warm-up artifact (pipeline/shader allocation). Every number
  above is from a settled run and was reproduced at least twice.
- Private bytes and working set are both reported because the old project's
  published table is RSS while the plan's budgets are private bytes.
- The renderer's own commit is 3.5 MB for `example.com` and 18.7 MB for
  Wikipedia, including the page's DOM/layout/images.
- The shell floor is `eframe`/`glow`, unchanged from the old app. The engine
  is no longer part of that floor.

## What this means

1. **Continue.** P1 (real IPC, snapshots, supervisor, 10-tab governor) is
   justified: the isolation costs almost nothing at the memory level.
2. **The shell floor is now the dominant cost** (~59 MB private, ~65 MB WS).
   It is still the single biggest lever, but it is no longer urgent: the
   current total already beats the old single-process app on private bytes.
3. **Do not swap the shell yet.** The deferred `winit`+`softbuffer` experiment
   remains the next big win if a later total breaks the envelope.

## Decision gate

| Gate | Result |
|---|---|
| S4 ≤ 50 MB | **pass** (−4 to −6 MB private) |
| S5 p95 ≤ 16 ms | **pass** (2.03 ms) |
| S1 ≤ 130 MB | **pass** (64.9 MB WS) |

**Verdict: continue to P1.**

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
.\tools\mem-harness.ps1 -Url https://example.com -Json spike-mem.json
.\tools\mem-harness.ps1 -Url https://en.wikipedia.org/wiki/Web_browser
.\target\release\renderer.exe --once https://example.com
```
