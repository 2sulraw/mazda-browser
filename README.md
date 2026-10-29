# Mazda Browser

A browser engine written in pure Rust from scratch, with one primary goal:
**minimal RAM**. No JavaScript engine in v1 — the scaling curve matters more
than the baseline (near-flat as tabs grow).

## Features

- **HTML tokenizer → arena DOM** — nodes stored as `u32` indices, element tags
  interned as `TagId(u16)`
- **CSS subset** — class/id/type/attribute selectors, descendant and child
  combinators, specificity + source-order cascade, inline `style=""`,
  `display:none`, `opacity:0`, media queries evaluated once at parse time,
  `@layer` entered transparently, CSS escapes in selectors (Tailwind-style
  `.data-\[state\=closed\]:hidden`)
- **External stylesheets** — `<link rel=stylesheet>` fetched in document
  order alongside inline `<style>`
- **Layout & rendering** — CPU rasterizer (software framebuffer), word wrap,
  underlines, link hit-testing, click navigation, back history
- **Text** — charset detection (header → `<meta charset>` → UTF-8), gzip,
  CJK/Hangul font fallback (range-gated so pages without those scripts pay 0)
- **Disk-backed HTTP cache** — `%LOCALAPPDATA%\mazda-cache`, 5-min TTL,
  bounded file count, size cap

## Non-goals (v1)

JavaScript, GPU compositor, process sandbox, full CSS, media playback,
extensions, DevTools.

## Build & test

```
cargo build --release
cargo test
```

Run with a URL:

```
target\release\mazda.exe https://example.com/
```

## Plan

See [PLAN.md](PLAN.md) for the frozen architecture plan, phase status,
measurements, and known gaps.

## License

[MIT](LICENSE)
