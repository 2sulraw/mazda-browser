# Mazda Browser

A browser engine written in pure Rust from scratch — HTML parser, CSS cascade,
layout, CPU rasterizer, and a JavaScript interpreter. One primary goal:
**minimal RAM**, with the scaling curve mattering more than the baseline
(near-flat as tabs grow).

No third-party engine code: the engine is ours. General-purpose utility
codecs (image decoders, font containers) are adopted when they are pure Rust
and permissively licensed.

## Features

- **HTML → arena DOM** — tokenizer builds nodes as `u32` indices, tags interned
  as `TagId(u16)`; declarative shadow DOM (`<template shadowrootmode>`) expanded
  post-parse; inline `<svg>` reassembled and handed to the SVG rasterizer
- **CSS** — class/id/type/attribute selectors (all operators), descendant/child
  combinators, `:not()`/`:is()`/`:where()`/`:has()`, `:first-child`/
  `:nth-child()`, `:link`, specificity + source-order cascade, inline `style=""`,
  `@layer`, `@supports`, `@media` (evaluated once at parse time), `var()` with
  theme scoping, `light-dark()` + `color-scheme`, `calc()`, CSS escapes
  (Tailwind-style `.data-\[state\=closed\]:hidden`)
- **Layout** — block flow, flexbox (row/column, wrap, grow/shrink/basis, order,
  auto margins), grid (fr/px/% tracks, `repeat()`/`minmax()`, named lines),
  relative/absolute/fixed/sticky positioning, `min-height`, borders (per-side,
  radius rings), padding, `box-shadow`, `clip-path: inset()`, text-align and
  line-height
- **Rendering** — CPU software rasterizer (no GPU compositor), word wrap,
  underlines/overline/line-through, per-box backgrounds, `:hover` restyling,
  cursor changes, link hit-testing, click navigation, back history
- **Images** — PNG, JPEG, WebP (VP8/VP8L/VP8X + alpha), SVG (hand-written
  rasterizer: paths, arcs→cubics, transforms, fills, strokes, gradients as flat
  midpoints), `srcset`; two-pass loading (header probe sizes the box pre-layout,
  pixels decode straight to display size), 8M display-px/page budget
- **Text & fonts** — charset detection (header → `<meta charset>` → UTF-8, BOM
  aware), gzip, CJK/Hangul system fallback (range-gated so pages without those
  scripts pay 0), `@font-face` webfonts (WOFF1 always, WOFF2 behind a feature),
  font-family stacks, per-page decoded-font budget
- **JavaScript** — from-scratch tree-walking interpreter: closures, classes
  (`extends`/`super`/static/fields/accessors), template literals, optional
  chaining, logical assignment, labels, `**`, destructuring, ES modules
  (`import`/`export` over HTTP), Promise microtasks, JSON/Math/Array/Object/
  Date/RegExp/URL builtins, DOM queries + mutation, `addEventListener` with
  bubbling, `preventDefault`, `fetch`, timers, `localStorage`; depth-capped so
  bad scripts error instead of crashing the tab
  - Optional **engine B**: a full JS engine core (`boa_engine`) selectable in
    settings, compiled in behind the `engine-b` cargo feature
- **Networking** — HTTP(S) via `ureq`, gzip, disk-backed cache
  (`%LOCALAPPDATA%\mazda-cache`, 5-min TTL, bounded files + size), persisted
  cookie jar (4096-byte cookie cap) sent on every request, `document.cookie`
  subset
- **Shell** — `eframe`/`glow` window, tab strip (new/close/switch), address bar
  (URL or DuckDuckGo search), back button, scrollbar, hover states, form
  controls (text/password/checkbox/radio/button/textarea/select), F11
  fullscreen, live RSS meter, settings overlay (⚙)
- **Tab snapshots** — the parsed DOM is serialized and restored on tab
  switch/back, skipping fetch + parse
- **Background loading** — fetch/decode/parse/cascade/layout run on a worker
  thread, so the UI keeps painting during navigation

## Non-goals (v1)

GPU compositor, process sandbox, full CSS, media playback, extensions,
DevTools. JavaScript is in scope, but heavyweight JS applications
(google.com-class) are not a near-term parity target: that needs a
Chrome-scale API surface and a JIT. DRM/media are parked (see
[PLAN.md](PLAN.md) for the reasoning and the ladder).

## Build & test

```
cargo build --release
cargo test
```

Optional features (combine freely):

```
cargo build --release --features woff2      # WOFF2 webfonts
cargo build --release --features engine-b   # engine B (boa) selectable
```

Run with a URL (or omit it for a blank tab):

```
target\release\mazda.exe https://example.com/
```

## Settings

The ⚙ button opens the settings overlay. Choices are persisted to
`%LOCALAPPDATA%\mazda.conf` and apply on the next navigation.

```
engine=a   # a = built-in pure-Rust interpreter (default), b = engine B
fonts=a    # a = WOFF2 webfonts, b = system fonts only
```

Resolution order, per setting: `--engine=`/`--fonts=` flag >
`MAZDA_ENGINE`/`MAZDA_FONTS` env var > `mazda.conf` > default `a`.
Engine B only runs in builds compiled with `--features engine-b`.

## RAM

Measured RSS on Windows (status-bar meter):

| Scenario | RSS |
|---|---|
| Idle window (`glow` + winit floor) | ~106 MB |
| Wikipedia article | ~100 MB |
| MDN reference page | ~80–95 MB |
| 1 active tab, more tabs restored from snapshots | grows with live pages only |

The `glow`/winit GL context set the current floor (a software-framebuffer shell
was ~15–20 MB but Windows-only). The near-flat curve as tabs grow is the real
design goal: hibernated tabs cost a serialized DOM, not a live renderer.
Every adoption is measured; see the measurements log in
[PLAN.md](PLAN.md).

## Plan

[PLAN.md](PLAN.md) is the frozen architecture plan: phase status, RAM targets,
measurements, known gaps, and recorded decisions (script runtime, font loading,
crate policy, DRM).

## License

[MIT](LICENSE)
