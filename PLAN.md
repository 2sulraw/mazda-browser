# Mazda Browser — Frozen Plan (v1)

Status: FROZEN 2026-10-27. Change requires updating this file in the same commit.

## Goal

Pure-Rust browser engine, written from scratch (no third-party engine code),
**no JavaScript in v1**, primary success metric: **lowest possible RAM** — the scaling
curve matters more than the baseline (near-flat as tabs grow).

## Architecture decisions (frozen)

- Single process, no per-tab process overhead
- **CPU rasterizer** (software framebuffer) — avoids the ~60–100MB GPU floor
- **No JS engine** in v1 — the second-biggest RAM lever after process model
- Runtime interpretation of HTML/CSS into Rust data (not compile-to-Rust per page —
  see "Recommendations" below for why)
- Arena DOM (`u32` indices), tag interning (`TagId: u16`), streaming-friendly tokenizer
- Disk-backed: HTTP cache, hibernated-tab snapshots. RAM-resident caches have hard LRU caps
- Images: keep compressed bytes, decode only visible, hard cap (~32MB), drop offscreen

## RAM targets (acceptance criteria per phase)

| Scenario | Target |
|---|---|
| Idle window | < 20 MB |
| One real site | ~40–80 MB |
| 1 active + 10 hibernated tabs | < 120 MB |
| 50 tabs (2 active) | ~150 MB — near-flat scaling |

## Phases

- [x] **Phase 0** — window + fetch + raw text + live RAM meter. *Measured: 38.5 MB (Wikipedia)*
- [x] **Phase 1** — HTML tokenizer → arena DOM → styled atoms → glyph-accurate word-wrap.
  *Measured: 80.0 MB (Wikipedia), flat over 16s, 10/10 tests*
- [x] **Phase 1b** — ~~CSS subset (class/id/attr selectors, inline `style=""`, cascade)~~
  **DONE: `css.rs` — selectors precompiled once per stylesheet into cascade-sorted rule
  list (Recommendations §2), right-to-left matching, specificity+order cascade, inline
  style, `display:none`, page background-color (36 tests; Wikipedia parses its real
  16.5KB sheet → 75 rules at +0 MB)**. Remaining: external `<link>` stylesheets,
  per-box background, pseudo-classes
- [ ] **Phase 2** — ~~links/clicking/history~~ **DONE: href attributes → hit-test →
  click navigation → Backspace back-history (16 tests; Wikipedia click→nav→back verified)**,
  ~~lazy font loading~~ **DONE: ab_glyph migration + OnceLock variants (Wikipedia 79.8 → 24.1 MB)**,
  ~~charset/gzip~~ **DONE: gzip already on (ureq default, 1MB→158KB wire); charset decoder
  added — header → `<meta charset>` sniff (1024B) → UTF-8, BOM-aware (encoding_rs, 6 tests,
  live GBK page verified)**, ~~font fallback (Phase 2b)~~ **DONE: per-glyph
  msyhl.ttc (han+kana, 11.6MB) → malgunsl.ttf (hangul, 4.9MB), range-gated so
  Thai/Arabic pay 0 (25 tests; GBK page renders 中文 correctly)**,
  ~~external stylesheets~~ **DONE: `<link rel=stylesheet>` fetched in document
  order alongside `<style>` (nextjs.org: 4 sheets → 260 rules)**,
  ~~HTTP disk cache~~ **DONE: `%LOCALAPPDATA%\mazda-cache`, FNV key, 5min TTL,
  oldest-mtime eviction at 300 files, 5MB body cap (Wikipedia reload = 3 hits, no network)**,
  ~~images with LRU decode cap~~ **DONE in Phase 1e (PNG+JPEG, probe-then-decode,
  8M px/page budget)**
- [x] **Phase 1c (Track A)** — box tree + flex/grid layout: ~~flat atom flow~~ **DONE:
  `BoxNode` tree (block/flex/grid groups + text leaves), `BoxStyle` cascades from CSS
  (display/gap/justify/align/width/margin-auto/text-align), block flow with legacy
  paragraph-gap semantics, flex row (max-content sizing, proportional shrink,
  justify, cross-axis align), flex column (row-gap), grid (fr/px/pct tracks,
  repeat()/minmax(), row-major cells), text-align per line (58 tests; nextjs.org
  hero centered + feature cards stacked, RSS 19.2 → 16.9 MB)**. Known gaps:
  no flex-wrap, no min-width, no position:\*, no flex-grow/basis,
  stretch doesn't reflow children
- [x] **Phase 1d** — box chrome (visual parity): ~~bare text boxes~~ **DONE:
  padding (border-box model) + per-side margins (block/flex/grid stacking via
  `place()` outer advance), line-height (`Style.lh`, unitless/%/px; default
  size×1.35 preserved), rem/em font sizes (rem = 16px root), per-box
  background-color → filled rects drawn under text (collected parent-first),
  border-radius (all corners, px/% vs width), rgba() colors (alpha 0 =
  transparent, >0 opaque — no compositing; 65 tests; nextjs.org hero spacing +
  terminal bg visible, RSS 15.7 MB)**. Known gaps:
  no box-shadow, no per-corner radius, no background-image, no alpha blending
- [x] **Phase 1e** — `<img>` rendering: ~~invisible~~ **DONE: PNG+JPEG decode
  (`png` + `jpeg-decoder`, both pure Rust), two-pass loading — header probe
  (IHDR/SOF) sizes the box pre-layout, pixel decode happens after layout and
  resamples straight to display size so full-resolution buffers never
  accumulate (8M **display** px/page budget charged at decode time — what
  stays in RAM, not intrinsic size; declared width/height attrs still reserve
  space when fetch/decode fails, aspect-ratio height, width attrs as
  presentational hints; content-negotiated Accept gets PNG/JPEG back from
  image proxies; 73 tests; local test page: intrinsic/CSS-width/
  attr-stretch/missing-file all correct)**.
  Known gaps: no WebP/AVIF/GIF (nextjs.org: 8/9 images decode — remaining
  skip is format), no srcset/art-direction, no lazy loading, no data: URIs,
  nearest-neighbour scale only
- [x] **Phase 1f** — SVG rasterizer for logos/icons: ~~skip~~ **DONE
  (`src/svg.rs`, hand-written — probe reads width/height/viewBox, render
  rasterizes at display size so vectors are pixel-exact at any scale):
  path (M/L/H/V/C/S/Q/T/A/Z abs+rel, arcs→cubics), rect/circle/ellipse/
  polygon/polyline/line, g/transform stack (matrix/translate/scale/rotate/
  skew), nonzero+evenodd scanline fill, stroke (quads + round joins),
  presentation attrs + inline style inheritance, named/hex/rgb()/rgba()
  colors, 2×2 supersampling, gradients collapsed to flat midpoint of stop
  colors; 81 tests; nextjs.org 8/9 images (SVG logos visible, was 1/9)**.
  Wave 27 adds the rest of the paint model: nested `<svg>` viewports,
  `clipPath` (+ objectBoundingBox units), `mask` (luminance × alpha),
  `pattern` (units/transform/objectBoundingBox), `use` (shapes, groups,
  `symbol`/`svg` with viewBox), and filters (`feGaussianBlur`, `feOffset`,
  `feFlood`, `feColorMatrix`, `feMerge`, `feBlend`, `feComposite`,
  `feDropShadow`) — see the wave 27 findings.
  Known gaps: no text/glyphs, no `image`, no CSS <style> blocks, gradients
  flat (not interpolated), no `marker`, no stroke-dash/miter/linecap
  variants, `mask-type: alpha` unread, pattern content painted once per tile
- [x] **Phase 1g** — borders: ~~missing~~ **DONE (`border`/`border-width`/
  `border-color` shorthands + per-side `border-top/right/bottom/left`,
  style words ignored = everything solid, unspecified color = currentcolor
  resolved from the element's text color at paint time): border widths join
  the border-box
  inset in layout (content starts inside borders, explicit width includes
  them, natural/wrap width grows by them), painted as 4 side strips over the
  background — or, when the box also has border-radius + background, as a
  ring (border-box fill with border color, background inset over it, inner
  radius ≈ r − border); 84 tests; nextjs.org terminal box border visible)**.
  Known gaps: one color for all sides (no per-side color), dashed/dotted
  render solid, no box-shadow, square border corners on rounded
  backgroundless boxes
- [x] **Phase 1h** — positioning: ~~missing~~ **DONE (`position: relative/
  absolute/fixed` + `top/right/bottom/left` + `inset` shorthand, sticky →
  relative shortcut): absolute/fixed leave normal flow (skipped by
  block/flex/grid, shrink-to-fit natural width, stretch when both horizontal
  insets), phase-2 `position_all` walk resolves insets against the nearest
  positioned ancestor's padding box (fixed: the viewport) after flow is
  final; relative shifts its subtree + stored run/underline ranges in place;
  fixed subtrees paint flagged (render double-pass: scrolled content first,
  fixed overlays on top in viewport coords, hit-test scroll-aware); 91 tests;
  nextjs.org renders with positioned elements)**.
  Known gaps: static position approximated by the containing-block origin for
  no-inset absolute boxes, no vertical auto-stretch (top+bottom+auto height),
  no z-index (paint order = DOM order), sticky has no scroll threshold
- [x] **Phase 1i** — box-shadow: ~~missing~~ **DONE (outer `box-shadow` list,
  any token order, rgba/8-digit-hex alpha, spread, multi-shadow, currentcolor
  default; layout emits `ShadowRect`s under backgrounds, render paints a
  blurred rounded rect per shadow with smoothstep falloff over blur/2 in the
  fixed/scrolled double-pass; `inset` shadows dropped; 95 tests; nextjs.org
  renders with elevation shadows)**.
  Known gaps: `inset` (inner) shadows dropped, no shadow-per-side, blur is a
  smoothstep approximation of the gaussian (not separable-blurred)
- [x] **Phase 1j** — font-family stack: ~~bool mono~~ **DONE (`Fam {Sans,
  Serif, Mono}` on Style; `font-family` walks the comma stack, first
  recognized generic wins — mono/courier/consol/menlo/monaco/code → Mono,
  sans/arial/helvetica → Sans (checked before `serif` so `sans-serif`
  doesn't misread), georgia/times/garamond/palatino/cambria/constantia →
  Serif; lazy Georgia load in Fonts; unknown webfonts fall through to Sans;
  96 tests)**.
  Known gaps: no bold/italic serif faces (regular Georgia only), no per-stack
  real-font matching (Arial/Verdana/Helvetica all → default sans), no
  generic `ui-rounded`/`system-ui` distinction
- [x] **Phase 1k** — flex-wrap: ~~missing~~ **DONE (`BoxStyle.wrap`,
  `flex-wrap: wrap|wrap-reverse` parses, `nowrap`/default = old
  shrink-to-fit; greedy line breaking at natural widths, per-line
  justify + cross-axis align + row-gap, `flex_row` refactored into
  `flex_line` per line; 99 tests)**.
  Known gaps: `wrap-reverse` wraps but doesn't reverse the cross axis, no
  per-line `align-content`, items never shrink on a wrap line (CSS `flex-shrink`
  inside wrapping lines ignored)
- [x] **Inline-boundary spaces** — text split across inline elements (`create `
  + `<strong>high-quality</strong>`) lost its separating spaces (empty split
  pieces at atom edges were skipped). Fixed: boundary/whitespace-only atoms
  emit space atoms (no-op at line start); regression test.
- [x] **Phase 1l** — CSS custom properties (`var()`): ~~none~~ **DONE
  (`--name` decls survive `parse_decls`, collected into one page-wide
  `Sheet.vars` map in document order; `resolve()` substitutes
  `var(--name[, fallback])` recursively (depth-capped cycle guard) before
  `apply_decl`; `hsl()`/`hsla()` color parsing for the `--ds-*` theme
  values; 102 tests)** — nextjs.org pill/backgrounds/borders now render
  via `var(--ds-*)`.
  Known gaps: flat page-wide var map (no per-element cascade/inheritance,
  `:root` vs `.dark` resolved by sheet order only), inline `style="--x:…"`
  custom props ignored, no `lab()`/`color()` colors
- [x] **Phase 1m** — inline `<svg>`: ~~dropped (NON_RENDERED)~~ **DONE
  (`svg` joins RAW_TEXT — parser keeps raw inner markup as a text child;
  `build()` reassembles `<svg attrs>…</svg>`, substitutes `currentColor`
  with the cascaded color, feeds it through the existing ImgSlot pipeline
  (markup-as-src, no URL fetch); svg.rs `get_attr` case-insensitive since
  the HTML parser lowercases attr keys; 103 tests)** — nextjs.org hero
  NEXT outline + pill copy icon + nav logos now rasterize (24 images, up
  from 8).
  Known gaps: nested `<svg>` inside `<svg>` truncates at the first close
  tag, `<foreignObject>` not skipped, `currentColor` substitution is
  string-level (nested CSS custom props inside the SVG ignored).
  Follow-up DONE (SVG class-paint round): site CSS coloring an inline-SVG's
  children by single-class rule (`.logo__letter { fill: … }`) now reaches the
  rasterizer — `parse_decls` keeps the paint props (added to `KNOWN`),
  `Sheet::class_paint` resolves last-in-cascade var()-substituted +
  `light-dark()`-picked values, and `build()` injects them into each open tag's
  inline `style` (which svg.rs prefers over presentation attributes). MDN's
  logo went from solid black to blue letters + light wordmark. shortcut: simple
  single-class selectors only (no descendant chains / tag / id / attr /
  pseudo); SVGs with no matching rule pass through byte-identical (verified
  nextjs + rust-lang unchanged)
- [x] **Selector-list split fix** — ~~footer/nav text piled at top-left~~ **DONE
  (`split_selectors` replaces `sel_text.split(',')`: commas inside `(...)`/`[...]`
  and backslash-escaped commas are literal; before the fix, `.prose :is(h1,h2,
  h3,h4)[id]{position:relative}` minted a bare `h4` rule that matched every
  `<h4>` on nextjs.org and yanked the footer columns to (0,0); 106 tests)**
- [x] **min-height + flex-column justify** — ~~hero NEXT outline overlapped the
  nav~~ **DONE (`BoxStyle.min_height` floors a box's content height in `place()`;
  a flex column with `justify-content: flex-end|center` pushes its stack into the
  freed space; nextjs.org hero `min-h-[560px] justify-end` now floats the NEXT
  outline above the pill; 108 tests)**
  Known gap: `%` min-height resolves against width, not container height
- [x] **UA defaults + cross-site parity fixes** — ~~Wikipedia/nextjs/rust-lang/MDN/HN
  each broke a different way~~ **DONE. UA `BG` → white `0x00ffffff`, default text →
  black, `load_page` bg = body → `<html>` → BG (body wins per HTML spec); `display:
  inline-block|inline-flex|inline-grid` → real box (buttons/badges paint); inline
  horizontal margins emit one separator space (HN `.hnname` gap); `font-size`
  honours `inherit/unset/initial` + `clamp()`/`vw`/`vh`; `parse_tracks` is paren-aware
  (skips whole unresolvable `calc(`/`max(`/`min(` groups — no phantom 720px track)
  and records `[line-name]` boundaries; named `grid-column: <name>` placement
  (`BoxStyle.col_lines`+`grid_col`, `grid()` resolves named spans); whitespace-only
  runs no longer become grid/flex items; wrapping flex rows fit available width
  (`natural_w_inner` caps at `avail`). Verified: HN header gap, MDN wide hero,
  rust-lang full nav + yellow button + black title, Wikipedia heading size — all
  match the reference browser; 113 tests; `f057898`)**
  Known gap: inline margins approximate to one space (not pixel-exact).
  Follow-up DONE (inline-block round): author `display:inline-block`
  now maps to `Display::InlineBlock` and sits ON the text line (shrink-to-fit,
  `block()`'s inline branch), so buttons/badges paint AND flow inline; only
  `inline-flex`/`inline-grid` still stack (honest `shortcut:` — no inline
  flex/grid layout mode yet)
  Follow-up DONE (calc round): `parse_len` now evaluates `calc()` arithmetic
  (`eval_calc`, a recursive-descent evaluator) to px — `+ - * /` with correct
  precedence, parenthesized sub-expressions, px/em/rem terms and unitless
  multipliers (`var()` substituted upstream). Previously any `calc(...)` was
  `None` → unset, so MDN's a11y skip-link menu (`top: calc(var(--offset)*-1)`)
  rendered visibly instead of being pushed off-screen. shortcut: `%`/`vw`/`vh`
  still unset (need a container/viewport `parse_len` doesn't have); no nested
  `calc(`. Verified nextjs/rust-lang/wikipedia/HN pixel-unchanged
- [x] **var() theme scoping** — ~~Wikipedia rendered dark with invisible text
  (night-scoped vars leaked past the `clientpref-day` root)~~ **DONE: `Sheet.var_defs`
  records every custom-prop definition with its selector; `scope_vars(&Dom, html)` rewrites
  the flat `vars` map after the DOM is built, dropping only definitions whose selector
  asserts a theme state the real `<html>` root class list contradicts. Wikipedia (root
  `clientpref-day`) drops `clientpref-night` dark vars → light; nextjs (no root theme)
  keeps its JS-era `.dark` values → dark; MDN (no root class) unchanged → dark.
  shortcut: only root-declared themes are modeled — a JS-activated theme is not a
  contradiction, so it stays (matches this machine's system-dark render).
  Verified: Wikipedia light, nextjs dark, MDN unchanged, HN + rust-lang no-op;
  115 tests; `241757d`)**
- [x] **font-size cascade: replace-against-parent** — ~~Wikipedia "Welcome to
  Wikipedia" `<h1>` rendered ~395px (each word overflowing the viewport)~~ **DONE:
  `font-size` `%`/`em` now resolve against the true parent size and REPLACE the
  cascade (spec) instead of multiplying into the running total; `inherit`/`unset`
  restore the parent, `initial` → 16. The cascade had compounded `UA(2×) × 188% ×
  1.8em × 1.8em` (a comma-rule whose two selectors both matched applied twice) and
  `inherit` froze the inflated value. `resolve` is a thin wrapper delegating to
  `resolve_with_parent(..., base.size)`; `build_into` passes `style.size` — the
  pre-UA true parent — so an author rule replaces the UA heading default rather than
  scaling it. Verified: Wikipedia heading ~32px + full featured page; rust-lang /
  MDN / nextjs / HN identical (no regression); 117 tests)**
  Known gap: a single comma-rule that matches twice still runs both selector arms
  (harmless now that font-size replaces, but other properties still double-apply)
- [x] **color-scheme + light-dark() + @supports** — ~~MDN rendered light while the
  reference renders dark (its `color-scheme: light dark` + `light-dark()` colours
  were dead)~~ **DONE: `PREFERS_DARK` global feeds `media_feature`
  (`prefers-color-scheme: dark|light`), `used_scheme(declared, root_pol,
  PREFERS_DARK)` resolves each element's scheme (own decl → inherited ancestor →
  root theme → OS pref), and `light-dark(a,b)` picks the branch in colour-bearing
  arms; `@supports` now evaluates for real (`supports_matches`: top-level
  not/and/or, `property:value` knownness from `KNOWN`) so native branches are
  entered and `@supports not` polyfills skipped. Follow-up fix: Wikipedia's
  `@media(prefers-color-scheme:dark)` OS-palette vars (selector
  `html…clientpref-os`, polarity-neutral) were winning the flat var map on its
  day root — `scope_vars` now lets any definition whose selector *actually applies
  to the real `<html>` element* outrank other scopes (gated on a root-declared
  theme, so nextjs/MDN JS-era behaviour is unchanged). Verified: MDN dark =
  reference, Wikipedia fully light, HN normal; 126 tests; `cc7db81`)**
  Known gap: `light-dark()`/`color-scheme` only affect colour arms (border,
  box-shadow included); `outline-color` unsupported entirely; `@supports`
  `property:value` tests property knownness only (a value we can't parse is
  still claimed supported — benign: those decls no-op)
- [x] **font-family stacks + `@font-face` webfonts** — ~~hero rendered Georgia
  instead of Alfa Slab One; `parse_fam` threw family names away; `@font-face`
  skipped whole~~ **DONE: `Sheet` interns every distinct `font-family` comma
  stack into a table shared with `Fonts` (`Rc<RefCell>`; `Style.fam_stack:
  u16` id, cascade interns the var-substituted value at resolve time — inline
  styles included), `collect_font_faces` parses each sheet's `@font-face`
  blocks against that sheet's own base URL (raw-slice scanner, NOT
  `parse_decls` — src paths must keep case), and `pick_font_for` walks the
  stack in CSS order before the generic pick: page webfonts (lazy
  `fetch` + `woff::decode_font`, disk-cached, per-face `OnceLock`, cleared on
  navigation) then installed-family aliases (verdana/tahoma/calibri/…, lazy
  one-slot-per-family). WOFF1 via `src/woff.rs` (`8f0e0b8`). Verified:
  rust-lang hero = Alfa Slab One (21 KB woff), body Fira Sans, HN Verdana,
  Wikipedia pixel-unchanged; 140 tests; `74eb2f3`)**
  Known gap: WOFF2 (`wOF2`) decodes to None → MDN (Inter) and nextjs (Geist)
  still use generics. WOFF2 stays unsupported (verified `a182f8f`): the one
  pure-Rust WOFF2 decoder mandates `safer-bytes ^0.2.0`, whose only 0.2.x
  returns a different error type than the decoder expects (never compiled;
  can't be pinned into working), and the maintained alternative ships a
  shaping engine duplicating the one we already use. No bold/italic
  variants for installed aliases (regular file only); webfont face selection
  is a coarse (weight-range, italic) bucket — no synthesis
- [x] **form-control chrome** — ~~`<select>` rendered as bare unboxed text and
  `<input>` rendered nothing at all~~ **DONE: `control_chrome()` fills UA
  border/background/padding/radius only where the cascade left them initial
  (author CSS always wins), controls join the surrounding text line
  (`BoxNode.inline` + line-continuation in `block()`, shrink-to-fit, vertical
  centering), a closed `<select>` shows ONE line — the selected/first option
  label + `▼` — options/optgroups never recurse into the flow, `<input>`
  gets value/placeholder text (password → bullets, hidden skipped,
  checkbox/radio → 14px box), `<textarea>` its default text, 170px default
  width for empty text controls. Bonus: hidden option labels are never laid
  out (rust-lang's language row no longer pulls in the CJK font → ~14 MB,
  was ~28 MB). Also fixed: `font-family: inherit` (normalize.css ships it
  on selects) keeps the parent stack instead of interning the word
  "inherit" as a family. Follow-up DONE: `BoxStyle` now carries a
  `declared` bitmask ([`css::decl`] flags) set whenever an author/inline
  rule touches border/border-color/background/padding/border-radius/
  min-height/width, so `control_chrome()` fills only properties the author
  did NOT declare — an author reset (`border:none`, `padding:0`,
  `background:none`) now suppresses the UA chrome, and a color-less
  `border:1px solid` renders currentcolor instead of the UA gray.
  150 tests; `f80f646` + `346de80` + this)**
  Known gap: `multiple`/`size>1` selects and textarea `rows`/`cols` render
  as the closed single-line control (no popup/listbox widget yet); the
  arrow is the ▼ text glyph, not theme-drawn art; checkbox/radio have no
  widget art)
- [x] **UI shell: eframe/glow** — ~~minifb framebuffer window + hand-drawn chrome
  (titlebar/tabs/address drawn into the page buffer; Win32 window controls)~~
  **DONE: app shell swapped to eframe 0.37 with the `glow` backend (NOT wgpu —
  wgpu would balloon the RAM floor; `default-features=false` keeps clipboard/
  persistence/accesskit/wayland-extra out), cross-platform winit window with
  native titlebar (min/max/close/resize/DPI for free on Windows *and* Linux —
  the per-OS Win32/X11 control layer became unnecessary and was deleted),
  egui-drawn chrome: tab strip (select labels, × close, + new, ≥1 tab,
  reload-on-switch {url,scroll} tabs), address bar (Enter → URL if it looks
  like a host, else DuckDuckGo search), Back button, link hit-test on the
  page texture, arrow/wheel scroll clamped, F11 fullscreen, RSS overlay;
  page still rendered by our own CPU rasterizer into one NEAREST texture,
  re-uploaded only on scroll/resize/nav; `render()` gained a `top` offset
  param (0 under egui); `working_set_mb` cfg-gated with a /proc VmRSS fallback
  + best-effort Linux font paths (161 tests; example.com verified live —
  tabs/Enter/F11/back/resize all exercised; `2473527`)**
  Known gaps: **RAM floor is now ~106 MB idle (vs ~15-20 with minifb) — the
  winit+glow GL context/IME stack costs ~100 MB; user accepted as the price
  for cross-platform chrome.** Native titlebar kept (borderless egui titlebar =
  later flip); 1 point = 1 texel (no per-monitor DPI raster scale); Linux
  paths compile-gated but never run on real Linux; wheel-scroll unverified
  live (synthetic events don't reach winit — arrow-key path verified instead)
- [x] **P1 parity wave (3 parallel worktree agents)** — three high-leverage
  rendering gaps closed at once: **(a) static pseudo-class selectors** —
  `css.rs` now parses/matches `:link`/`:visited` (any element with `href`;
  visit history not modeled), `:not(<compound>)`, `:is()`/`:where()`,
  `:root`, `:first-child`/`:last-child`/`:nth-child(an+b)` (from existing DOM
  parent linkage), class-level specificity (`:not` = its arg's, `:is` = best
  alternate, `:where` = 0); dynamic/state pseudos (`:hover/:focus/...`) and
  pseudo-elements still dropped whole (`00c1b7a` +8 tests) — **fixes HN's
  entire link palette (titles now black, no UA underlines), Wikipedia's
  underline-everywhere, MDN/wiki skip-link positioning**;
  **(b) declarative shadow DOM** — post-parse `expand_declarative_shadow_roots`
  in `html.rs` splices `<template shadowroot|shadowrootmode="open|closed">`
  children into the live tree (flat expansion; plain `<template>` stays inert;
  nested DSD terminates; `2a34274` +4 tests) — **MDN header search/theme/
  language UI exists for the first time (46 templates)**;
  **(c) inline backgrounds + `<br>`** — paint-worthy inlines (background/
  border/shadow/h-padding) promote to real inline-level boxes so the bg spans
  text+padding, `width:100%`/`%`-width resolved layout-locally in `layout.rs`
  (no css.rs change), single `<br>` advances exactly one line (no 0.6×size
  gap; 2+ brs each add one empty line); `7c7dc59` +4 tests.
  Verified visually: HN titles black, wiki serif h1 + clean links, rust-lang
  yellow button full-width with text inside, MDN hero single-gap; **177
  tests; merged `d289fa1` + `a5fe0f2`)**
  Known gaps spotted this round: MDN theme/language popup menus render
  permanently open (their hiding style unapplied) + empty outlined menu box
  top-right + MDN main nav tabs still absent (deeper than the hoisted
  templates); Wikipedia black search/Tools widgets (var-map, P1.5 debug
  session pending); HN vote arrows + orange full-bleed header (background-
  image + table % widths, P2); leading `<br>` no-op; `:visited`==`:link`
- [x] **Forms + scripts wave (3 parallel agents + integrator fixes)** — user-visible
  interactivity milestone: **(a) async shell** — the whole fetch/decode/parse/
  cascade/layout pipeline moved to a worker thread (mpsc job enum Load/Relayout/
  Event; NavGate generation guard; job coalescing; 10s/15s ureq timeouts —
  infinite default was the freeze; catch_unwind; worker Fonts; PageWire Send
  form; 150ms resize debounce, measured Wikipedia pipeline 6.6s;
  `511e8e0`+4 tests); **(b) form controls** — inputs/textarea/button/checkbox/
  radio render as real UA-default boxes, `FormState{focused,values}` node-id
  keyed survives re-layout, `hit_test_form`, password mask, tail-clip;
  `layout_in_forms` (6ca9247 +8 tests); **(c) shell wiring** — click = form
  hit-test first, letters/backspace route to the focused field (pure
  `route_key`), Enter logs `[form] submit`, Esc blurs; FormState rides every
  worker job; (d) **JS: from-scratch tree-walking interpreter** (js.rs +
  js/{lexer,parser,interp,builtins,tests}.rs: closures, classes-desugar,
  template literals, JSON/Math/Array/Object/Date/URI/timers/Promise-microtasks,
  24-byte Val, depth caps → Err not panic; `8b38556` +52 tests) and its
  **activation**: `<script>` inline+src run in document order on the worker,
  DOM natives in script.rs (`getElementById/querySelector/single-selector/
  createElement/append/attrs/textContent/innerHTML-text/className/
  addEventListener+bubbling/location/fetch-sync/setTimeout`), listener dispatch
  from click routing, worker keeps DOM+engine+CSS between jobs, engine dropped
  per nav; errors `[js]`-logged never fatal (`6d15851` +19 tests);
  **(e) dark-scheme text colors** — UA default text color flips black→white
  only when the page declares `color-scheme` dark and no author color
  cascaded; same fix repaired MDN's invisible dark logo; example.com h1 turned
  out to be absent from the source page (not our bug) (`3278e26` +2 tests).
  Verified live: Wikipedia runs real scripts (`[js] run inline/external`),
  typed fields focusable, logo dark-correct; **280 tests; master**
  `aa53bbf→…` merges + `3db8950` vh hookup + `0d8b866` wire-literal fields.
  Known gaps queued: MDN raw SVG source text paints in header (SVG escaping
  its element via DSD hoist), MDN header action row as text line, wiki
  "Jump to content" + jammed buttons, engine lacks regex/destructuring/
  async-await (scripts using them error-log), no form POST, RAM floor up
  (Retained DOM+engine: wiki 93.1 MB)
- [x] **Event-loop wave** — unified worker scheduler: timers + rAF frame
  epochs (16ms coalescing, 512-timer/128-frame backlog caps, drop-oldest
  with [js] log) + microtasks run to quiescence between/after jobs;
  **input events as script-visible**: keydown/input per keystroke, change
  on blur/Enter, scroll coalesced, click dispatch with worker-side
  checkbox toggling and preventDefault → Cancel/Navigate decision (pure
  fn, UI holds pending_link until the verdict); rAF → coalesced relayout
  signal; natives: addEventListener (global bucket), requestAnimationFrame,
  location.reload, localStorage/sessionStorage (worker-RAM), readyState,
  navigator.userAgent "Mazda/1.0", matchMedia, alert/confirm/prompt to
  stderr, console variants, full event objects; stopPropagation/
  cancelAnimationFrame/keyup deferred (`30945c8` +20 tests; wiki 100.8MB)
- [x] **Language-depth wave (Option A grows)** — from-scratch bounded regex
  engine (flat instruction program + iterative backtracking, MAX_STEPS 100k/
  MAX_BACKTRACK 8k, program/group caps, lookaround via bounded sub-run,
  512-entry literal cache; `(a+)+$` blowup returns in budget, never hangs;
  lookbehind/named-groups/`\p` = SyntaxError), real Promise state machine +
  chaining + all/race/any/allSettled combinators, async/await
  run-to-completion with bounded microtask drain (10k cap) and honest
  unsupported-error instead of silent hang, generators via mini-VM with
  explicit frames (+2 engine bugs fixed: loop redecl shadowing, resume
  feed), destructuring/spread/rest + class get/set accessors + instanceof
  fixes; verified on Wikipedia: 70KB startup bundle runs end-to-end
  (`26971f2` +39 tests; 339 total; wiki 97.1MB)
- [x] **Selectable script runtime (settings page)** — the agreed A/B choice is
  live: `ScriptRuntime` trait (5 methods: run_scripts/dispatch/run_due_timers/
  run_due_frames/next_wake_deadline + per-nav factory); `DomHost` installed
  per turn (DOM pointer + nav/dirty/preventDefault/storage side-channels);
  script.rs natives now thin host wrappers shared by both engines (zero
  behavior drift, 300-baseline tests untouched); **Option B = full engine
  core behind `--features engine-b` (default build pulls none of it,
  cargo-tree-verified)**: globals + 13 element methods + wrappers mirror
  A's native surface exactly, per-turn side-channel harvest, errors logged
  never fatal; **settings**: ⚙ button at the toolbar right end, overlay in
  dark tokens with EXACTLY the agreed copy:
  `JavaScript engine: (•) A — Pure Rust built-in (light) / ( ) B — Full JS
  engine core (compatibility, +30 MB/page)`; B row inert on feature-off
  builds; persisted to `%LOCALAPPDATA%\mazda.conf` (`engine=a|b`,
  whitespace/comments tolerated), precedence flag `--engine=` > env
  `MAZDA_ENGINE` > conf > default A (corrupt file → silent default A, never
  panic); engine applies on next navigation (no hot-swap, shortcut).
  RAM honest: A 94.7→B 96.3 MB (simple), DDG heavy-script page 82.2→176.2
  (B ASTs — the +30 is a typical-page ballpark, bundles exceed it); shipped
  release built WITH the feature so B is user-selectable (binary 7.5→15.7
  MB, B costs nothing until chosen). Tests: default 344, feature 346
  (`a95010d`; both contract-tested, one conf-parser bug found+fixed by tests)
- [x] **Gap-fix wave (form POST + layout + the MDN hang)** — four parallel
  threads, all general fixes:
  **(a) real form submission** — GET/POST from live DOM: checked-state from
  worker DOM + FormState overrides, urlencoded serializer (GET replaces the
  whole query, POST via uncached fetch adopted through the normal nav
  channel), two-stage submit veto (button listeners → form `submit`
  listeners) and native `form.submit()` (no event, no veto); live-proven
  against a local HttpListener (`de58108`, +3 tests);
  **(b) inline/flex layout** — plain inlines with horizontal margins are
  promoted to real inline boxes (real gap advance, no jamming), flex item
  slots span the whole margin box, `%` width is intrinsic during
  max-content measurement (cyclic sizing) and caps at the row's remaining
  share as a flex basis; absolute `top/left` verified unclamped (`8654f89`,
  +7 tests, live Wikipedia header fixed);
  **(c) label/margin semantics** — `contained_text` skips `svg`/`script`/
  `style` subtrees (icon `<svg>` in a `<button>` no longer leaks `<path…/>`
  source into the label), `margin` shorthand parses positionally WITH `auto`
  slots in place and layout honors per-side auto (both → center, left-only →
  push right, right-only → hug left) instead of any-auto → center
  (`148968b`, +3 tests);
  **(d) the MDN hang — two bugs deep**: (1) **interpreter had no step
  budget** — a `for(;;)` fed by a broken scanner pegged the worker forever
  (57s CPU/60s wall, page never completed); added a per-turn dispatch budget
  (`steps`/`tick()` in eval_stmt/eval_expr + the generator driver, reset per
  turn, uncatchable by script try/catch, budget error carries the spinning
  statement's line/col via span → line_col) — a runaway loop now aborts in
  ~1s with an honest `RangeError: script exceeded the step budget`; (2)
  **the actual spinner was the LEXER**: a literal `$` NOT starting an
  interpolation inside a template literal (`` `$${r}` ``) fell into the
  raw-run arm which breaks AT the `$` without consuming → infinite loop;
  fixed with a literal-`$` arm. With both fixes MDN's /CSS/margin completes:
  `[mem] after layout 86.4 MB (2877 runs)`, CPU 1.7s/60s wall (`b1640ed`,
  +8 tests; remaining script errors are honest unsupported-syntax reports,
  not hangs: `export const` module syntax, `\x` escape). Tests: default
  371, feature 374
- [x] **Wave 1 (parallel worktrees) — box-sizing + overflow culling** — first wave of
  the Wikipedia visual-parity plan (2 agents, separate worktrees, merged clean):
  **(a) `box-sizing`** — parsed into `BoxStyle.content_box` (was dropped at parse
  time by the KNOWN allowlist); `content-box` grows the border box by padding+
  border after `place()` resolves them, auto-margin centering uses the FINAL
  border box (pad/border resolution moved above the auto-margin block — no-op
  for border-box, byte-identical default), abs-pos stretch/shrink + flex `%-slot`
  pre-sizes restated through `as_content_decl()` so they don't double-add pads;
  `%` padding + content-box approximate (cyclic — px exact, commented);
  `5bfc09a`, +4 tests;
  **(b) `overflow`/`-x`/`-y` + `clip: rect()`** — parse into `BoxStyle.clip`/
  `clip_rect` (`hidden|clip|scroll|auto` cull; `scroll`/`auto` clip but don't
  scroll — commented); `collect_paint` threads a composed clip window (descendants
  clipped to the ancestor's padding box; own bg/border NOT clipped by own
  overflow; nested clips intersect), new `apply_run_clips` post-pass intersects
  the window into each subtree's `run_range`/`ul_range`, `build()` now
  blockifies `position:absolute/fixed` elements (the skip-link `<a>` previously
  flattened into flow — without this the pattern could never work), `render()`/
  `draw_text` cull fully-outside runs and glyphs; **the documented skip-link gap
  is closed** — live Wikipedia Main Page no longer shows "Jump to content" (was
  visible in every capture before); `c6f0070`, +6 tests.
  Tests: default **381**, feature **384** (2 parallel agents both branched from
  `875d5a4`; git auto-merged both `BoxStyle` halves, verified by full suite +
  field presence before push). Live: wiki 96.4 MB, CPU 0.9s (`18451f0` pushed).
- [x] **Wave 2 (parallel worktrees) — float/clear/vertical-align + background geometry**
  — second wave of the Wikipedia visual-parity plan (2 agents, both branched from
  `9e9f110`):
  **(a) `float`/`clear`/`vertical-align`** (`4353177`, +8 tests) — parse into
  `BoxStyle.float/clear/valign` (`Float`/`Clear`/`VAlign` enums); each `block()`
  owns a local `Vec<Flt>` in doc coords, seeded with ancestor floats threaded
  down from `place()`; a floated child is claimed before the inline branch, laid
  shrink-to-fit at the current line's top, advances neither flow y nor the open
  line; `line_excl()` subtracts overlapping floats' x-spans from each new line's
  range (zero room → drop below the floats' bottoms); `clear` raises flow y to
  the named floats' bottoms; floated inlines blockify in `build()` (mirrors the
  `position:absolute` pattern); flex/grid/abs subtrees get `&[]` — floats never
  reach them; `vertical-align` offsets inline boxes at line build (absent =
  byte-identical). Shortcuts (all `shortcut:`-commented): no upward float
  escaping from nested blocks, mid-line floats don't re-flow already-placed
  runs, no min-content floor, fixed sub/super metrics, text-align only exact
  beside floats when left-aligned;
  **(b) `background-size`/`-position`/`-repeat`** (`1922d3e`, +8 tests) — parse
  into `BoxStyle.bg_size/bg_pos/bg_repeat` (`BgSize`/`BgPos`/`BgRepeat` enums) +
  the `background:` shorthand parses repeat + `position/size` around a top-level
  `/`; the pin-to-top-left placeholder loop moved to the END of `place()` so
  %-sizes/%-positions resolve against the USED padding box; cover/contain are
  aspect-preserving against the padding box; position uses the standard
  `(box−img)×p` formula; declared repeat tiles one `collect_paint` entry per
  tile (region = padding box ∩ wave-1 clip window, capped 512/node). Shortcut:
  undeclared `bg_repeat: None` keeps the engine's long-standing single draw (the
  spec initial is `repeat` — sites rely on the current look, commented).
  Merged: A fast-forwarded, B's css.rs/layout.rs conflicts hand-resolved as pure
  unions (both sides only added new enums/tests/imports — verified by field +
  arm + KNOWN presence). Tests: default **397**, feature **400**.
  Live: wiki 105.4 MB (+9 MB vs wave 1 — tiled paint entries), CPU 0.5s;
  featured-article text wraps around the floated image, sidebar images float
  right, section headers paint sized backgrounds (`cf18ae4` pushed).
- [x] **Wave 3 (parallel worktrees) — `::before`/`::after` pseudo-elements + `content`,
  `z-index` + `transform`** — third wave of the Wikipedia visual-parity plan
  (2 agents, both branched from `3f20188`):
  **(a) pseudo-elements + `content`** (`59cd182`, +9 tests) — `parse_selector`
  accepts `::before`/`::after` AND legacy single-colon `:before`/`:after`
  (records `PseudoEl` on the last Seg; static pseudo-classes may precede,
  nothing may follow; `spec_of` adds +1 type; `seg_matches` ignores the field so
  `li::before` matches the `<li>`); pseudo rules are PARTITIONED at parse into
  `Sheet::pseudo` buckets so the origin element's `resolve`/`class_paint`/
  color-scheme never see them (zero hot-path cost; `resolve_pseudo` cascades a
  bucket over the origin's computed `Style`); `content` parses `none`/
  string/`attr(name)` raw onto `BoxStyle.content` (attr resolves at box
  synthesis where the DOM is); layout `pseudo_box` synthesizes an inline
  text-run/inline-box (bare run unless the pseudo paints chrome or declares
  box-level display) inserted FIRST (`::before`) / LAST (`::after`), no box
  for undeclared/`none`/`display:none`/empty. Shortcuts: counters → empty
  (no list markers), `attr()` bare names only, no inline-style pseudo rules,
  replaced elements/form controls generate no pseudo box, `::marker`/
  `::placeholder`/`::backdrop` still drop;
  **(b) `z-index` + `transform`** (`9db76da`, +11 tests) — `z-index: auto|
  <integer>` into `BoxStyle.z`; `collect_paint` stamps each rect/image/run
  with a stacking level (static inherits tree order; an integer-z POSITIONED
  box switches its whole subtree) and `zsort` stable-sorts the three paint
  channels by (z, tree order) with an all-equal early-out — pages without
  z-index are byte-identical; `transform` parses `none|translate[XY]|
  translate|scale` (rotate/skew/matrix parse but no-op — axis-aligned
  rasterizer, honest shortcut) and paints through a diagonal `Xf` matrix
  composed down the tree: whole-subtree paint moves incl. text anchors +
  clip windows mapped to paint space, layout flow untouched. Shortcuts:
  stacking-context scoping global, per-channel reordering only (hit-testing
  stays tree order), transform-origin fixed at center, glyphs not scaled.
  Merge: A fast-forwarded; B's css.rs conflict was a pure union (both added
  arms); layout.rs auto-merged — verified by field/arm/symbol presence +
  full suite. Tests: default **417**, feature **420**.
  Live: wiki **97.5 MB** (1468 runs — +215 synthesized pseudo boxes; RAM DOWN
  7.9 MB vs wave 2 despite more runs), CPU 0.8s. Note: the release build
  transiently crashed rustc (stack-buffer-overrun) under the prior
  disk-pressure episode; after the disk freed up it built clean in 2m45s
  with no code changes (`505a98b` pushed).
- [x] **Wave 4 + Wave 5 (four parallel worktrees, two waves in flight) — mask-image,
  `:hover` + `cursor`, `position: sticky`, typography batch** (mask and sticky
  branched from `62614f3`, hover and typography likewise; all merged into
  master as `c276ab5` → `cda4355` → `da5a2e5`):
  **(a) `mask-image`** (`26cd43e`, `18373b6`) — a GENERAL vendor-prefix strip at
  the declaration gate: a property starting with a single `-` (never `--`)
  drops through its second `-` and the remainder is looked up in `KNOWN`, so
  every prefixed spelling of a known property behaves exactly like the bare
  one (no vendor name is ever written down; `@supports` gates the same way).
  `mask`/`mask-image`/`mask-size`/`mask-position`/`mask-repeat` reuse the
  background geometry parsers; a mask URL rides the existing image pipeline as
  a second placeholder child (`is_mask`) and counts against the same pixel
  budget; paint suppresses the box's own bg rect + bg image and draws the mask
  image tinted with the box's bg color (RGB replaced, alpha kept, tinted once
  and cached) — untinted when no bg-color. Shortcuts: masks the box's own
  background only (text/children unmasked), one layer, no
  composite/mode/clip/origin.
  **(b) `:hover` + `cursor`** (`2cf19a3`, `dbc7313`) — `Pseudo::Hover` parses
  (other dynamic pseudo-classes still drop); Hover-carrying rules partition
  into `Sheet::hover` composing with the pseudo bucket (`li:hover::before`);
  a `Cell<Option<u32>>` on the sheet names the hovered node (`None` = nothing
  matches), and `Hover` matches the node or an ancestor/descendant of it.
  Layout stamps each element box's DOM id (`BoxNode.node`) and emits
  `Layout::hover_targets` (doc rect + node + effective `cursor`), reverse-scanned
  by `hit_hover` with the same scroll math as link hit-testing; the shell reads
  the pointer position only, and on a changed hovered set re-cascades + rebuilds
  from the cached DOM (forms preserved) and repaints. `cursor` parses to a
  keyword enum, inherited down the box tree, mapped to the OS pointer only
  inside the page rect. Shortcuts: full rebuild per hover change (no
  invalidation), hit-testing in tree order (z/transform not reflected),
  over-matching on ancestor+descendant walks.
  **(c) `position: sticky`** (`1661fe4`) — `Pos::Sticky` is its own variant and
  every match treats it as IN FLOW (a declared `top` never offsets layout, the
  anti-relative guard); `collect_paint` records one `StickyGroup` per sticky box
  that declares `top`: subtree spans across all paint channels (rects/shadows/
  images from DFS push windows closed after the children, runs/underlines from
  `run_range`/`ul_range`, plus the decoded-image span filled by the shell's
  decode pass) with `doc_y`/`doc_h`, px `top`, the parent's bottom as the
  container edge; per frame
  `offset = clamp(scroll + top - doc_y, 0, container_y1 - (doc_y + doc_h))`
  shifts every entry in the group, z order untouched. Shortcuts: top-only
  stickiness, nested groups shift independently, container = parent box (not
  the spec scrollport), hit-test/form rects unshifted, no re-clipping at the
  container edge.
  **(d) typography batch** (`0aadf15`, +11 tests) — `text-transform` maps the
  string once when text boxes are built (ASCII-only case ops, `capitalize`
  = first ASCII letter per whitespace-separated word); `letter-spacing` /
  `word-spacing` resolve onto `Style` and are added by ONE shared rule
  (`spacing_extra`) in the line builder and replayed per glyph in the draw
  path, so measurement and paint never disagree; `white-space`
  (normal/nowrap/pre/pre-wrap/pre-line) picks the atom splitter — nowrap never
  wraps, `pre` keeps whitespace and turns `\n` into hard breaks, `pre-wrap`
  keeps and wraps, `pre-line` folds but breaks; `text-indent` shifts the first
  line of its declaring block (negative allowed). Shortcuts: ASCII-only case
  mapping, no script/CJK spacing rules, per-node white-space, block-level
  indent only, no `break-word`/`overflow-wrap`, form labels skip
  text-transform (live user input).
  Merge: mask + sticky + typography landed with hand-resolved unions
  (dual-channel `collect_paint` signature, `decode_images` keyword union of
  `parent_bg`/`parent_masked`/`next`, `Atom` word/`br` helpers threaded with
  hover `node`/`cursor`, `Layout` field unions). Tests: default **450**,
  feature **453**. Live: wiki Main_Page **93.6 MB** (1618 runs, DOWN 3.9 MB vs
  wave 3 with +150 runs of preserved typography), Rust article 102.7 MB /
  14740 runs; both render clean (`da5a2e5` pushed).
- [x] **Wave 6 (three parallel worktrees) — CSS list markers, WebP decoding, WOFF2
  webfonts** (all branched from `b204bf0`; merged `262fc44` → `22517ad` →
  `41c4018`, then WOFF2 `d9aba58` → `36d4b35`):
  **(a) list markers** (`262fc44`, +16 tests) — `list-style-type` /
  `list-style-position` land on the inherited `Style` (so `ul`/`ol` UA defaults
  flow into `li` like a real UA sheet) with the UA defaults and the 40 px list
  gutter supplied by a new tag-driven `ua_box(tag)` (author `padding*` still
  wins); the `list-style` shorthand takes type/position/`url()` in any order
  (`url()` ignored, shortcut). Marker text is synthesized as the `<li>` box's
  first child (`•`, `◦`, `▪`, `1.`, `01.`, `a.`, `A.`, `i.`, `I.`), numbered
  from a per-list counter seeded by `<ol start>` and reset by `<li value>`,
  nested lists restart independently; `outside` hangs the marker left of the
  content box without advancing the line (wrapped lines align with the first
  line's TEXT), `inside` puts it in the first line's flow; `::marker` parses
  with its own pseudo bucket and honors only `content`/`color`;
  `content: counter(list-item)` resolves inside the shared content resolver.
  Shortcuts: the ten common types only, no `list-style-image`, no
  `@counter-style`/CJK/disclosure markers, no `reversed`, markers only for
  `<li>` tags, hidden items still consume a number.
  **(b) WebP** (`22517ad`, +6 tests) — adopt the pure-Rust `image-webp` codec
  (general-purpose utility crate, per the crate policy) and detect
  `RIFF....WEBP` exactly; header-only dimension probe for VP8/VP8L/VP8X,
  decode of static lossy, lossless and extended (alpha plane preserved) through
  the SAME display-size resample + per-image budget path as PNG/JPEG, into the
  engine's `0xAARRGGBB` buffer; animated files decode frame 0 only; malformed
  input is a counted skip, never a panic.
  **(c) WOFF2** (`38e54e9`, `d9aba58`, +4 tests) — adopt the pure-Rust `wuff`
  decoder behind the optional `woff2` cargo feature; `src/woff.rs` remains the
  single container dispatch point (`wOFF` → existing sfnt rebuild, `wOF2` →
  sfnt when the feature is on, None otherwise), with a decompression size
  guard. The frozen font setting ships verbatim: `Font loading:` `A — WOFF2
  webfonts (site fonts, +~10-15 MB/page)` / `B — System fonts only (minimal
  RAM)`, default A, precedence `--fonts=` > `MAZDA_FONTS` > conf `fonts=` >
  A, persisted on change and applied on the next navigation; a 32 MB
  decoded-font LRU evicts oldest-first and rejects single faces over the cap,
  and B (or a build without the feature) skips the webfont fetch/decode
  entirely, failed faces are negatively cached. Shortcuts: budget is
  per-instance (2× worst case), decoder-coverage gaps fall back to system
  fonts, no unicode-range subsetting/font-display semantics.
  Tests: default **476**, `woff2` **477**, `engine-b + woff2` **480**.
  Live: wiki Main_Page **106.9 MB** (1646 runs; +13.3 MB vs wave 5 — exactly
  the WOFF2 webfonts the UI copy advertises — with site serif typography now
  active in headings and body, images decoding, zero format skips); wiki list
  bullets `•`/`▪`, `<ol start=3>` → `3.` `4.` and `<li value=7>` → `7.` all
  verified on screen; WebP verified live through a local HTTP page (VP8X +
  ALPH fixture decoded at display size, alpha region visible, 0 format skips).
  MDN `/Web/CSS/list-style-type`: markers render but its grid still collapses
  its sidebar to a narrow column (pre-existing grid-track gap, not marker
  related). Note: `data:` image URLs are skipped as an unknown format for
  EVERY format (PNG included) — a pre-existing image-source gap, unrelated to
  WebP (`36d4b35` pushed).
- [x] **Wave 7 (three parallel worktrees) — real table layout, object-fit/aspect-ratio,
  modern image sources** (all from `91cb007`; merged `cd76ec5` → `02fba80` →
  `0794a6b`):
  **(a) table layout** (`cd76ec5`, +13 tests) — tables were plain block stacks
  (every cell full-width, vertically stacked); now a `TableRole` drives a real
  grid: rows collected from `<tr>` and from `thead`/`tbody`/`tfoot` in visual
  order (caption above), cells placed on columns, UA 2px `border-spacing` +
  1px cell padding + bold/centered `th`. `table-layout: auto` sizes each column
  to its content: the per-column max of each cell's max-content width and its
  min-content (widest unbreakable word + padding/border), `colspan=n` spreading
  its width over n columns, declared `width` (CSS px and the legacy attribute)
  acting as a column floor; the table shrink-wraps to the column sum, and when
  that exceeds the container the columns shrink proportionally but never below
  their floors (water-filling), so a long word overflows instead of squashing.
  `table-layout: fixed` takes the first row's declared widths. `separate`
  honors `border-spacing`; `collapse` removes the gaps and overlaps adjacent
  cells' borders by the wider side so the shared edge paints once. Cell
  `vertical-align` (CSS or the `valign` attribute) works against row height.
  Shortcuts: `rowspan` parsed but not laid out, no anonymous cells, percentage
  widths only as a linear share, `caption-side: bottom`, collapsed-border
  conflict resolution approximate (later cell in tree order wins), author
  `display: table*` not modeled (`colgroup`/`col` dropped).
  **(b) object-fit / object-position / aspect-ratio** (`e08e946`, +6 tests) —
  `fit_content_rect()` maps the intrinsic size into the box for
  fill/contain/cover/none/scale-down with `object-position` anchoring (% or px);
  `cover`/`none` decode once at content size and `image::crop` extracts the
  visible window, so the draw entry is exactly the box intersection; a declared
  width or height plus `aspect-ratio` derives the other axis (plain boxes and
  images both). Shortcuts: padding box as the fit area, no `<video>`/`<canvas>`,
  nearest-neighbour only, `aspect-ratio: auto <ratio>` content half dropped,
  budget charged at content size.
  **(c) modern image sources** (`87ee69c`, +16 tests) — root cause of the
  `data:` skip: every image URL went through `resolve`, which rejects non-http
  schemes, so the bytes never reached the sniffer; `resolve_img` now passes
  `data:` through untouched in probe AND decode (PNG/JPEG/WebP/base64 or
  percent-encoded SVG all decode), and CSS `url(data:…;base64,…)` no longer
  breaks declaration splitting. `srcset`/`<picture>` are resolved at build
  time into the ONE url the image box carries: the first `<source>` whose
  `type` is decodable and whose `min/max-width` media query matches, else the
  `<img>`'s `srcset` (smallest `w` ≥ the slot width, `x` → 1x, `sizes` first
  parseable length else viewport), else `src`. IMAGE Accept now advertises
  `image/webp`. Shortcuts: viewport-width-only selection (no DPR, no `sizes`
  media conditions), media queries limited to width, no AVIF/JPEG-XL, no lazy
  scheduling.
  Tests: default **511**, `engine-b + woff2` **515**. Live: wiki Rust article
  **112.2 MB** (14973 runs; infobox now a real two-column label/value grid
  instead of a stacked list, 315 images, 0 format skips); a data-URL page
  renders both a PNG and a WebP `<img>` inline; `<ol start>`/bullets verified
  earlier. Note: a `data:` page re-laid out ~10× in 18 s (no pointer input) —
  not seen on http pages (2-3×), filed as a shell wake-up quirk to watch
  (`0794a6b` pushed).
- [x] **Wave 8 — real grid track sizing, CSS math, page scrollbar** (grid
  `715625e`, math `8b6b96f`, shell `9f4ba99`, merge `7b66e0c`):
  **(a) grid tracks** (`715625e`, +12 tests) — `TrackBreadth`
  (`Px/Pct/Fr/Auto/MinContent/MaxContent`) and
  `Track::{...,MinMax(a,b)}`; `minmax()` was previously parsed as only its
  maximum side (a `minmax(15rem,1fr)` lost its 240 px floor — the MDN
  sidebar-collapse class). Two-phase sizing: base pass resolves fixed and
  content floors from the item's **margin-box** min-content (widest
  unbreakable word, intrinsic replaced width) and max-content (one line),
  `minmax` floors apply before free-space split; fr tracks then share the
  remaining space weighted and clamp to their maxima; a grid whose min-content
  exceeds the container overflows instead of collapsing tracks. `gap` /
  `row-gap` / `column-gap`, percent tracks, `min-content`/`max-content`, and
  `grid-template-rows` floors are honored. Shortcuts: `grid-column: span N`
  not implemented, `repeat()` counts capped at 64, `auto-fill`/`auto-fit`
  skipped, no baseline alignment/subgrid/masonry, `fit-content()` skipped.
  **(b) CSS math** (`8b6b96f`, +5 tests) — one recursive evaluator for
  `calc()`/`min()`/`max()`/`clamp()` (additive < multiplicative < unary,
  nesting depth 32): px/%/number linear form, `%` carried symbolically in the
  new `Len::Calc { px, pct }` and resolved by `layout::resolve_len` against the
  containing block (so `calc(100% - clamp(…))` works); division by zero,
  non-finite results and unknown functions drop the declaration (initial
  value), `clamp` with `min > max` yields the min; every `parse_len` caller
  picks it up (width/height/margins/padding/borders/inset/font-size/
  letter- and word-spacing/text-indent/background geometry). Math groups in
  grid tracks resolve to a single px/% track; a px+% mix is skipped (no mixed
  track form). Shortcuts: mixing px with % ACROSS `min/max/clamp` arguments
  drops, no `round/mod/abs/sign/trig`, no `ch/ex/lh/cq*`, math inside
  `repeat()`/`minmax()` args not expanded, box-shadow/bg-position use the px
  part of a mixed result.
  **(c) page scrollbar** (`9f4ba99`) — the new shell paints a faint track and a
  proportional thumb in the right gutter after the page raster
  (`scrollbar_thumb`/`scroll_from_thumb`/`scrollbar_strip`, thumb floored at
  24 px), with thumb drag (grab offset kept), track click (thumb centres under
  the pointer), wheel and arrow keys all routed through one `set_scroll`
  helper; a press inside the bar strip no longer falls through to link
  hit-testing and the page `cursor` does not leak across it. Also fixed a
  latent crash this exposed: a resize repaint could reallocate `pixels` at the
  new size while `last_size` still described the old texture, tripping the
  color-image length assertion — `repaint_page` now syncs `last_size`. Runs
  entirely in the shell, so page memory is unchanged.
  **(d) `display: contents` + named grid areas** (`80cba80`, +5 tests) — a
  `display: contents` element generates no box: `build_into` hoists its
  children into the parent's flow, so a grid/flex parent sees them as its own
  items, and inline contents join the surrounding line with no wrapper.
  `grid-template-areas` (quoted rows, `.` = empty cell, a name repeated across
  cells spans them) and `grid-area: <name>` now place an item into its cell
  rect; a name with no matching cell falls back to auto placement, and a grid
  with no explicit columns gets one content-sized `auto` track per area
  column. Row-spanning items paint from their start row but do not stretch the
  rows they cover (shortcut: no span-aware row distribution — a tall spanning
  sidebar can overhang its last row); numeric/`span` `grid-area` forms and
  named *lines* beyond `grid-column: <name>` are still not modeled.
  Tests: default **536**, `engine-b + woff2` **540**. Live: wiki Rust article
  **105.0 MB** (14973 runs, 315 images); MDN `/Web/CSS/list-style-type`
  **88.8 MB** (4687 runs) — the sidebar + article + "In this article" TOC now
  form a real three-column layout (previously the whole page collapsed into
  one narrow column because `display: contents` and named areas were missing);
  remaining visual gap: TOC/sidebar link rows overlap their background bands.
  The scrollbar draws and follows the document (verification capture only;
  drag was not synthetically driven). Next: the Wikipedia h1/languages-box
  overlap and the MDN link-row spacing.
  **(e) outer box-shadow hole** (`dab4cbf`) — a blurred shadow rect was painted
  over the element's whole outer footprint, including the area *behind* the
  box, so any box whose own background was transparent or semi-transparent
  (cards, sticky sidebars, popovers) showed a gray band through itself. The
  layout now records the element's border box in `ShadowRect.hole` (mapped
  through the transform), and the paint pass culls shadow pixels inside that
  rounded box before falling off — `box-shadow` stays strictly outside, as
  CSS specifies. One extra `ShadowRect` field, no extra passes.
  Tests: default **536**, `engine-b + woff2` **540**. Live: MDN
  `/Web/CSS/list-style-type` **88.7 MB** (4687 runs) — the gray overlap band
  under the sidebar/TOC is gone; remaining gaps are column widths and the
  search/filter widget stacking.
  **(f) media math, real `:is()` arguments, UA block tags, logical box
  shorthands** (`73722e6`, +7 tests) — five independent fixes that all showed
  up on the same MDN page:
  - `@media` lengths go through the CSS math evaluator and viewport units
    (`calc()`, `min()/max()/clamp()`, `vw/vh/vmin/vmax`), so MDN's
    `(width < calc(1rem*2 + 15rem + 2rem + 31rem))` gate (800px) and its
    1072px three-column gate now evaluate instead of failing the parse and
    dropping the whole block. `vh` resolves against a fixed 700px (the same
    single-window assumption the width already makes; no re-evaluation on
    resize).
  - `:is()` / `:where()` accept FULL selector chains per argument
    (`:is(.toc a):not(:hover)`, `:is(.card summary) .pill`,
    `:is(:is(...) a)`), not just compound selectors. Sites write the
    `text-decoration:none` rule for all sidebar/TOC/breadcrumb links this way;
    dropping it underlined every navigation link on the page. Specificity is
    the best argument's whole-chain specificity.
  - unlisted HTML block tags (`details`, `summary`, `dialog`, `menu`,
    `legend`, `optgroup`, `option`, `search`, `hgroup`, `colgroup`, …) are
    block-level in the UA sheet. An unstyled `<details>` used to take the
    inline path and shrink-wrap to max-content: the MDN baseline card
    measured 766px inside a 704px column. Now 704.
  - logical box shorthands (`padding-inline`, `padding-block`,
    `margin-inline`, `margin-block`, and their `-start/-end` longhands) map to
    physical sides for horizontal-tb, including the `margin-inline: auto`
    centering pair. MDN's page grid gets its side padding from
    `padding-inline`; without it content sat flush against the window edge.
  - `:first-of-type`, `:last-of-type`, `:only-child` (needed by
    `h1 code:first-of-type` for the title chip and by menu rules), and
    intrinsic sizing that honors a declared px `width` and sums the items of a
    non-wrapping flex ROW: the 80px search button had measured at its label's
    min-content (18px), so its `min-content` grid track collapsed and the
    button overflowed the header.
  Development aid: `MAZDA_DUMP_BOXES=2` walks the laid-out box tree
  (tag.class, rect, display, position, tracks, declared width), and
  `MAZDA_DUMP_RUNS` also prints run widths/underline flags and the underline
  channel.
  Tests: default **543**, `engine-b + woff2` **546**.
  Known shortcut this exposed: custom properties still come from ONE global
  last-declaration-wins map, not a per-element/inherited cascade, so a sheet
  that defines the same variable for several variant classes (MDN's
  `.baseline-indicator.high` green vs `.removing` red) paints every variant
  with whichever block appears last — the "Widely available" pill is red.
  Per-element custom-property cascade is the next CSS wave.
  **(g) flex-grow + the `:nth-*-of-type` family** (`83cd0a5`, `fa27dd2`) —
  `flex-grow` (longhand and the first slot of the `flex:` shorthand; `none` =
  0, `auto` = 1) distributes a non-wrapping row's free space among growable
  items by factor *before* `justify-content` runs. Plain Wikipedia relies on
  `flex-grow: 1` on the page heading inside a `justify-content: flex-end`
  title bar: without it the title box sat at x=528 (pushed right against the
  language menu) and wrapped to two lines; it now starts at the bar's left
  edge and stays on one line, as the reference shows. `flex-basis` and
  `flex-shrink` are still not modeled (the base width is the item's natural
  size). `:nth-last-child()`/`:nth-of-type()`/`:nth-last-of-type()` (sharing
  the `an+b` evaluator with `:nth-child`) unblock MDN's
  `.breadcrumbs li:not(:nth-last-child(-n+2))` trim rule and common
  table/list striping.
  Tests: default **548**, `engine-b + woff2` **552**. Live (release,
  `engine-b + woff2`): MDN `/Web/CSS/list-style-type` **83.2 MB** (2943 runs;
  baseline card 704px, no stray TOC/breadcrumb underlines, side padding and
  title chip present, search button inside its min-content track); plain
  Wikipedia Rust article **103.9 MB** (14973 runs, 315 images; page heading
  now starts at the title bar's left edge on one line).
  **(h) relayout image memo** — a relayout (JS mutation, event, hover) rebuilds
  the page from scratch, and every one of them re-fetched, re-decoded,
  re-downscaled and re-cropped the page's images: 315 decodes × every hover
  step on the Wikipedia article. `KeptPage` now owns an `ImgCache` keyed by the
  exact decode inputs (source hash, display size, mask tint, `object-fit`
  content rect, visible window); a hit reuses the same `Rc<ImageData>`, so the
  old and new page share one buffer and the decode (the expensive part) never
  repeats. Verified live in a debug run with 27 relayouts: the first layout
  decoded 315, every later one reported `315 drawn (0 decoded, 315 from the
  relayout memo)`. Bounded at 6 M display px (under the 8 M page budget) with
  oldest-insert eviction; it dies with the page, so a navigation frees it.
  Shortcut: the memo trusts the bytes it decoded — an image that changes
  server-side under the same URL keeps the old pixels for that page's lifetime
  (a reload refetches).
  Tests: default **549**, `engine-b + woff2` **553**. Live (release): Wikipedia
  article **98.7 MB** first layout (14973 runs; the earlier 104 → 118 MB climb
  was this decode churn, so it no longer reproduces without pointer input).
  **(i) per-element custom-property cascade** — `--*` declarations used to land
  in ONE page-wide last-wins map, so a sheet that defines the same variable
  per variant class painted every variant with whichever block came last:
  MDN's `.baseline-indicator.high` green pill rendered in `.removing`'s red.
  Each rule now carries its own `--*` declarations (var-only rules keep an
  empty `decls` and live in a separate cascade-sorted `var_rules` list), and
  `resolve_scoped` builds the element's `VarScope` chain: its own matching
  rules (cascade order, last wins) → parent chain → the page-wide palette as
  the floor. An element that declares nothing reuses the parent's `Rc`, so the
  common element costs no allocation; layout threads the chain down
  `build_into` and `build_scoped` hands the `<body>`'s own chain in.
  Inline `--*` on the `style` attribute joins the chain too.
  Live: MDN's "Widely available" pill is green (it was red); page otherwise
  unchanged (656 rules, 2943 runs, 85.5 MB debug); Wikipedia article
  unchanged (14973 runs, 106.8 MB debug).
  shortcut: a pseudo-element's `var()` still resolves from the page-wide
  palette, not its origin element's chain; the flat map remains what the
  `:root`-theme polarity filter (`scope_vars`) rewrites, so theme scoping is
  unchanged.
  Tests: default **550**, `engine-b + woff2` **554**
  (`custom_properties_cascade_per_element_and_inherit`).
  **(j) flex per-line `flex-grow` + auto margins** — free space in a flex row
  is now given to growable items on EACH line of a wrapped row instead of
  being skipped for `flex-wrap` containers (the old shortcut: "wrapped rows
  grow per line in a real engine; not modeled"). Wikipedia's
  `header.vector-header` is `flex-wrap:wrap`, so its `div.vector-header-end`
  (`flex-grow:1`, natural 247 px) never widened and the account links /
  search box sat mid-header; it now grows to 667 px and right-aligns them.
  `margin-left/right:auto` also absorb the line's free space before
  `justify-content` (CSS flex layout), so an auto-margin item pins to the
  edge whatever `justify` says.
  Live: Wikipedia article header right-aligned (14973 runs, 107.4 MB debug);
  MDN unchanged (656 rules, 2943 runs, 81.5 MB debug).
  shortcut: cross-size `align-items: stretch` still does not reflow.
  Tests: default **552**, `engine-b + woff2` **556**
  (`flex_row_wrap_grows_each_line`, `flex_auto_margins_absorb_free_space`).
  **(k) `:has()`, attribute operators, exact-fit wrapping** — `:has(...)`
  parses as a relative selector list: a leading `>` / `+` / `~` anchors the
  argument's leftmost compound to a child / the next element sibling / any
  later sibling, no combinator means any descendant; specificity follows
  `:is()`. That recovered MDN's `.pill:has(+.browsers)`,
  `li:has(details)`, `:not(:has(>li>details))` and
  `.breadcrumbs-bar:has(…)` rules (656 → 664 rules on the CSS reference
  page). Attribute selectors gained `~=`, `|=`, `^=`, `$=` and `*=`
  (`[class*=interactive-example]` inside a `:has()` needed it).
  A word that exactly fills its line no longer wraps: the shrink-to-fit
  max-content width and the line's per-word sum differ in the last float
  digit, and the bare `>` split MDN's uppercased, letter-spaced "WIDELY
  AVAILABLE" pill onto two lines (the comparison now allows 0.01 px).
  Live: MDN CSS reference unchanged apart from the one-line pill (664 rules,
  2773 runs, 84.2 MB debug); Wikipedia article unchanged (1646 rules, 14973
  runs, 101–118 MB debug across runs — RSS noise).
  shortcut: `:has()` honors only a LEADING combinator inside the argument (a
  `+`/`~` between compounds isn't tokenized yet); the attribute `i` flag is
  not modeled; `:has()` walks the candidate subtree per element match.
  Tests: default **554**, `engine-b + woff2` **558**
  (`has_pseudo_matches_descendant_child_and_siblings`, `attr_selectors`,
  `word_filling_the_line_exactly_does_not_wrap`).
  **(l) `<button>` is a container** — the form-control branch flattened
  every `<button>` into one synthetic text leaf, so any markup inside it
  vanished: MDN's declarative-shadow search button (an inline `<svg>`
  magnifier) rendered as an empty 80x30 pill, and `button::before` icon
  techniques (hamburgers, chevrons) generated nothing. A button now builds
  a real group box, recurses into its children (icons, styled spans,
  `::before`/`::after`), and keeps the `FormCtl { Submit }` on the box so
  clicks still activate and submission still reads the label text. UA
  `text-align: center` moved onto the box in `style_for`, so children
  inherit the centering and an author `text-align` still wins.
  Live: MDN's header now paints the magnifier (3 -> 11 images drawn, 664
  rules, 2773 runs, 91.0 MB debug); Wikipedia Rust article unchanged (1646
  rules, 14973 runs, 315 images, 106.7 MB); rust-lang.org unchanged
  (1753 rules, 483 runs, 88.7 MB); a data:-URL probe with an inline
  stroke-only magnifier inside a flex button went from "no child box" to
  "svg box at the button's left inset".
  shortcut: `<input type=submit|button|reset>` stays a leaf (its label is
  the `value` attribute by definition); button `type` is always Submit.
  Tests: default **557**, `engine-b + woff2` **561**
  (`button_lays_out_its_children`, `button_keeps_generated_content`,
  `author_text_align_overrides_the_button_default`).
  **(m) `clip-path: inset()`** — the modern visually-hidden idiom
  (`position:absolute; width:1px; height:1px; overflow:hidden;
  clip-path:inset(50%)`, used by Tailwind's `sr-only` and Radix/Chakra)
  parsed to nothing, so hidden labels painted. `clip-path` now parses
  `inset(t r b l)` (1/2/3/4 values, px and %, `round` dropped) into
  `BoxStyle::clip_path`; the layout clip window is the border box inset by
  those edges, intersected with any legacy `clip: rect()` (which stays
  positioned-only, per CSS 2.1). `clip` also accepts the legacy
  space-separated `rect(0 0 0 0)` form. An inline that declares a clip now
  forces a box (`paints_chrome`) — flattening it away lost the window.
  Live: a probe page with two `sr-only` labels paints only the two visible
  strings; MDN unchanged (664 rules, 2773 runs, 11 images, 76–91 MB debug).
  shortcut: other shapes (`circle`/`ellipse`/`polygon`/`path`/`url()`),
  `round` radii, `margin-box`/`content-box` reference boxes, and hit-test
  clipping are not modeled.
  Note: Wikipedia's header labels ("Main menu", "Search") still show —
  its static CSS carries no `position:absolute` for
  `.vector-dropdown-label-text`; the reference gets the 1px box from a
  class its JS adds, so that one is a script-runtime gap, not a CSS one.
  Tests: default **560**, `engine-b + woff2` **564**
  (`clip_path_inset_parses_edges`, `clip_path_inset_hides_visually_hidden_labels`,
  `clip_path_inset_maps_each_edge`).
  **(n) `<sub>`/`<sup>` + the rest of `text-decoration`** — `sub`/`sup`
  rendered as ordinary inline text (no shift, no size change) and only
  `underline` existed, so `<del>`/`<s>` showed no strike. UA now gives
  `sub`/`sup` `font-size: smaller` (~0.83em) plus `VAlign::Sub/Super`
  (`ua_box`), and an inline that declares a baseline shift forces a real
  inline box — plain inline flow flattened it away and the shift was lost.
  `text-decoration` gained `line-through`/`overline` (and the
  `text-decoration-line` longhand); the decoration rect channel paints
  them at mid-line / 1px below the line top.
  Live: a probe page renders H₂O, mc², `[1]`, struck `$99`/`$49`,
  overline and a combined strike+underline run; Wikipedia's Rust article
  is unchanged (1646 rules, 14973 runs, 315 images, 96–116 MB debug) and
  its `[n]` markers are now raised and smaller.
  shortcut: decoration `color`/`thickness`/`offset` are not modeled (the
  text color and 1px are used), and `text-decoration` still does not
  propagate to descendant inline boxes the way the spec's decoration
  propagation does.
  Tests: default **563**, `engine-b + woff2` **567**
  (`sub_and_sup_shift_the_baseline_and_shrink`, `del_and_s_paint_a_strike_line`,
  `text_decoration_line_parses_through_the_cascade`).
  **(o) Phase 3 groundwork — DOM snapshots** — a tab switch re-fetched and
  re-tokenized the page. `html::Dom` now serializes to bytes (magic `MDOM`,
  version, interned tag table, per-node parent/children/kind, u32
  length-prefixed text + attribute strings, trailing FNV-1a hash) and back;
  `from_bytes` validates every index, tag id, UTF-8 string and the
  parent⇄child agreement, so truncation, a flipped byte, trailing junk or a
  wrong version are all a clean miss. The worker writes the PRE-SCRIPT tree
  to `%LOCALAPPDATA%\mazda-cache\dom\<url hash>` after a normal GET (48
  files, oldest-mtime eviction, 16 MB per-file cap; `data:` URLs are never
  stored and error pages are never snapshotted), and `LoadReq::Load` carries
  a `restore` flag: tab switch / close / back and startup pass true and skip
  fetch + parse, then re-collect stylesheets (`styles_for`, split out of
  `parse_source_with_styles`) and **re-run the page's scripts**, so listeners
  and dynamic content behave exactly as on a fresh load.
  Live: the Wikipedia Rust article restored from a 1347 KB snapshot —
  worker time **11.8 s → 4.1 s**, identical 19239 nodes, 1646 rules,
  **14973 runs**, 315 images, 105–114 MB debug (no RAM change; the snapshot
  never lives on the heap after load); example.com 684 → 43 ms, 25 runs.
  Found + fixed while verifying: a still pointer kept re-hovering after each
  relayout (the hovered box shifts its children out from under it, the next
  hit test lands on a sibling, forever — one full article layout per 4 s).
  `hover_should_relayout` now dispatches only on pointer motion or a press;
  the busy/scrolling guards stay.
  shortcut: the snapshot is keyed by URL only (the same TTL bet the HTTP disk
  cache already makes), the restored tree is the pre-script one so scripts
  replay (listeners are never serialized), and stylesheets are still
  re-fetched from the HTTP disk cache on restore.
  Tests: default **567**, `engine-b + woff2` **571**
  (`snapshot_round_trip_preserves_the_tree`,
  `snapshot_rejects_corrupt_or_truncated_files`,
  `snapshot_rejects_inconsistent_links`, `hover_relayout_needs_pointer_motion`).
  **(p) Flex model completion** — the row model only had `flex-grow`, a
  natural-size base and a uniform squeeze when a line overflowed, so
  `flex-shrink: 0` items still shrank, long words were squeezed below their
  min-content, `flex: 1 1 0` gave the longer text more space than the
  shorter, and `order`/`align-self`/`flex-flow`/`flex-basis`/`min-width` were
  dropped at parse time. `BoxStyle` gained `shrink` (initial 1), `basis`
  (px/%/auto), `min_width`, `order` and `align_self`; the `flex` shorthand is
  now complete (`none`/`auto`/bare length/`grow [shrink [basis]]`, a bare
  number always the grow factor) and `flex-flow` sets direction + wrap.
  Layout: `items_of` stable-sorts by `order`; a declared `min-width` floors
  the used width in `place()`; the row computes each item's basis, then
  `shrink_row` distributes negative free space by `shrink x base` in up to
  four passes, clamping every item at its automatic minimum size
  (`flex_floor`: min-content of the CONTENT — a new `content_min_w` ignores
  the item's own declared width, which is the base being shrunk — capped by
  that declared width, and collapsed to the margin box when `overflow`
  clips); `min-width` overrides the floor. Every declared item width is
  pinned to the computed slot so `place()` cannot re-resolve it back to its
  authored size, and `align_self` overrides `align-items` on both the row
  line and the column cross axis.
  Live (debug, 6 sites): Wikipedia Rust article 14973 runs / 315 images /
  107.3 MB, MDN list-style-type 2773 runs / 81.9 MB, rust-lang.org 483 runs /
  82.3 MB (its GET STARTED label now sits inside the button instead of
  overflowing it), Hacker News 784 runs / 73.8 MB, nextjs.org 502 runs /
  99.5 MB, example.com 25 runs / 66.7 MB — no unexpected relayouts.
  shortcut: `row-reverse`/`column-reverse` keep the forward main axis;
  `align-self: stretch` does not reflow the item (same as `align-items`);
  `flex-basis: content` is treated as `auto`; `order` changes paint/layout
  order only, not tab/focus order.
  Tests: default **574**, `engine-b + woff2` **578**
  (`flex_shrink_shares_negative_space_by_base_size`,
  `flex_shrink_zero_gives_up_nothing`,
  `flex_shrink_stops_at_the_longest_word`,
  `flex_basis_zero_makes_grow_shares_equal`,
  `flex_order_reorders_without_touching_the_dom`,
  `align_self_overrides_align_items`,
  `flex_shrink_basis_order_align_self_and_flow_parse`).
  **(q) Cookie jar** — every request was anonymous: `Set-Cookie` was thrown
  away and `document.cookie` did not exist, so a login POST never came back
  logged in. New `src/cookie.rs` implements an RFC 6265 subset (name/value,
  `Domain` host-only vs suffix match, `Path`, `Max-Age`/`Expires` with an
  RFC 1123 date parser, `Secure`, `HttpOnly`), sends the matching `Cookie`
  header on every GET/POST (page, stylesheet, script, image, font), captures
  every `Set-Cookie` value through `resp.all` (a login sets several at once)
  and persists to `%LOCALAPPDATA%\mazda-cache\cookies`. The jar is capped at
  3000 cookies with 4096 bytes per cookie (expired evicted first, then
  oldest-created), the file is
  written at most once per second, and a change made inside that second is
  **deferred** (dirty flag flushed from the frame loop) rather than dropped —
  dropping it lost a script's `document.setCookie` on every page that also
  received a `Set-Cookie`. Script sees `document.getCookie()` /
  `document.setCookie(str)` because the interpreter has no property
  accessors; HttpOnly cookies stay hidden and a script write cannot create
  one.
  Live: a local two-load probe — load 1 stored `srv=1` from the response and
  `js=2` from script; load 2 (a new process) sent `Cookie: srv=1; js=2` and
  the page printed `REQ: srv=1; js=2` / `JS: srv=1; js=2`. Wikipedia
  `/wiki/Web_cookie` (fresh) stored 6 cookies and sent them on every
  subresource (12860 runs, 97.6 MB); `/wiki/HTTP_cookie` in a new process
  sent those 6 from the persisted jar (12855 runs, 93.9 MB); MDN restored
  unchanged (2773 runs, 81.6 MB).
  shortcut: `SameSite` is parsed away (all requests are same-site here),
  `Expires` accepts RFC 1123 only (anything else stays a session cookie), no
  public-suffix list (a `Domain` must still be a suffix of the request host),
  cookies set on a redirect hop are lost (the client follows redirects
  internally), and the jar is one profile-wide store with no per-site
  partition.
  Tests: default **588**, `engine-b + woff2` **592**
  (`set_and_send_respects_path`,
  `domain_attr_covers_subdomains_but_not_siblings`,
  `secure_cookies_only_go_over_https`, `http_only_is_hidden_from_script`,
  `max_age_zero_and_past_expiry_delete`,
  `expiry_in_the_future_keeps_the_cookie`,
  `longer_paths_come_first_then_creation_order`,
  `cap_evicts_oldest_and_expired_first`,
  `text_round_trip_preserves_cookies`,
  `corrupt_persisted_lines_are_skipped`,
  `js_set_uses_the_document_directory_and_js_get_hides_http_only`,
  `oversized_cookies_are_rejected`,
  `malformed_set_cookie_lines_are_ignored`,
  `url_parts_handle_ports_userinfo_and_ipv6`).
  **(r) JS syntax batch 1** — real pages kept failing at PARSE time on
  syntax the engine reported as unsupported. Five constructs now parse and
  run:
  - **optional chaining** `?.` on a member, index (`a?.[i]`) or call
    (`f?.()`). The parser folds a postfix chain into one
    `ExprKind::OptionalChain { base, steps }`, so a nullish value at an
    OPTIONAL step ends the WHOLE chain as `undefined` (`a?.b.c.d` never
    throws when `a` is null), while a nullish value at a later
    non-optional step still throws. The base is evaluated exactly once and
    a call step keeps the preceding object as `this`
    (`o.get()?.v`, `o.m?.()`).
  - **computed object keys** `{[expr]: v}` and computed methods
    `{[expr](){}}` (`ObjElem::Computed`), key evaluated once at creation.
  - **object-literal accessors** `get x(){}` / `set x(v){}`
    (`ObjElem::Accessor`). `{get: 1}` and `{get(){}}` stay ordinary
    properties. A get/set pair in one literal (and in a class body) now
    merges into ONE accessor instead of the second half overwriting the
    first (`obj_set_own` merges two `Val::Acc` entries).
  - **exponentiation** `**` and `**=`, right-associative and tighter than
    `*` (`2 ** 3 ** 2` = 512).
  - postfix `++`/`--` moved from the `*` level into the postfix chain, so
    `x * y++` parses as `x * (y++)` (it previously errored on the
    product). `-2 ** 2` yields `(-2) ** 2` where the spec rejects it —
    the parser does not track parentheses (honest shortcut).
  Deferred with honest SyntaxErrors: `class extends`, `static` class
  members, BigInt literals, labeled statements/`break label`.
  Live: a `data:` page runs 16 hand-written checks covering every new
  construct — 16/16 print `ok` in the capture. MDN
  `/Web/CSS/list-style-type` is unchanged (2773 runs, 81.8 MB: no
  run-count or RAM regression); the Wikipedia `JavaScript` article (new
  URL) parses and lays out at 11404 runs / 100.8 MB.
  **(s) class completion** — real pages declare `class X extends Y` and
  the engine rejected the whole script. Classes are now complete enough for
  ordinary site code:
  - **`extends`** links both chains: instance methods through
    `Object.setPrototypeOf(B.prototype, A.prototype)`, statics through a
    new `Closure.proto` [[Prototype]] on the class function
    (`Object.setPrototypeOf(B, A)`), so `B.s()` finds `A.s` and
    `instanceof` keeps working. `Object.getPrototypeOf` added alongside.
  - **`super()` / `super.m()`** are resolved at PARSE time against a hidden
    `__superN` binding created next to the class, so a method can sit in a
    closure created in any scope. `super.m()` evaluates the superclass
    prototype (or the class itself in a static member) and calls it with the
    current `this`; a bare `super.m` yields the unbound function.
  - **static members**: `static m(){}`, `static x = v`, `static get/set`
    (installed on the class function; `get_member`/`set_member` now
    intercept `Val::Acc` there and walk the function proto chain).
  - **public instance fields** `x = v` / `x;` initialize at the top of the
    constructor — in a derived class right after the first top-level
    `super(...)` call (a super call nested inside an arrow is not detected:
    honest shortcut). Static fields run at definition time in source order.
  - a derived class with no constructor gets the spec default
    `constructor(...args) { super(...args) }`.
  Deferred with honest errors: private `#fields`, computed class keys,
  class expressions, `new.target`, and class redeclaration across separate
  `<script>` tags (the old binding silently wins).
  Tests: `class_extends_and_super`, `class_static_members`,
  `class_fields` (594 default / 598 with `engine-b woff2`). Live: a
  `data:` page runs 11 class checks — 11/11 `ok`. MDN
  `/Web/CSS/list-style-type` unchanged (2773 runs, 81.9 MB); the Wikipedia
  `JavaScript` article unchanged at 11404 runs.
  **(t) JS syntax batch 2** — the next set of constructs real bundles hit at
  parse time:
  - **optional catch binding** `catch { ... }` (ES2019). The block gets a
    non-lexable `#catch` binding so the statement shape stays uniform.
  - **logical assignment** `??=`, `||=`, `&&=` (`Op::CoalesceAssign` /
    `LogicalAndAssign` / `LogicalOrAssign`, `ExprKind::LogicalAssign`). The
    right-hand side is evaluated ONLY when the operator writes
    (`a ||= f()` never calls `f` when `a` is truthy) — a plain compound
    assign would have evaluated it eagerly.
  - **labeled statements** `label: stmt` plus `break label` /
    `continue label` (`Flow::Break(Option<Rc<str>>)`). A labeled loop takes
    its label on entry (`JsEngine::pending_label`) so `continue outer`
    restarts THAT loop; a labeled plain block only observes
    `break label`. `switch` now propagates a labeled break instead of
    swallowing it.
  - **class expressions** `const C = class ...` / `class extends A {}`
    (`ExprKind::ClassExpr`): the same lowering as a declaration runs in a
    private child scope, so a named class expression's binding does not
    leak out.
  - crash fix found on a live site: `Object.assign(fn, {...})` hit
    `unreachable!("builtin installed on non-object")` and killed the page
    load with a worker panic. Assign now writes through `set_member`, so a
    function/class target (or a primitive) is handled instead of panicking.
  Still deferred with honest errors: private `#fields`, BigInt literals,
  modules (`export`/`import`), `for await`, `new.target`.
  Tests: `optional_catch_binding`, `logical_assignment`,
  `labeled_statements`, `class_expressions`,
  `object_assign_accepts_function_target` (599 default / 603 with
  `engine-b woff2`). Live: a `data:` page runs 13 checks — the 8 newly
  supported print `ok`, the 5 deferred ones keep their honest errors.
  **svelte.dev now renders**: it was blank before because its inline
  scripts died on `catch {}`; the client-rendered hero lays out at 291 runs
  (115-126 MB with its large hero images). MDN
  `/Web/CSS/list-style-type` unchanged (2773 runs, 82.0 MB); Wikipedia
  `JavaScript` unchanged at 11404 runs / 100.1 MB.
  Tests: default **591**, `engine-b + woff2` **595**
  (`computed_object_keys`, `object_literal_accessors`,
  `optional_chaining`; the `**` assertions moved into
  `arithmetic_and_precedence`).
  **(u) ES modules** - `<script type="module">` is no longer ignored:
  - the parser treats `import`/`export` as *contextual* words (neither
    becomes a lexer keyword, so both stay usable as identifiers). Supported:
    default / named / `* as` / bare `import "m"`; `export const|let|var`
    (destructured names included), `export function`, `export async
    function`, `export class`, `export { a as b }`, `export { a } from
    "m"`, `export * from "m"`, and `export default <expr|function|class>`
    (an anonymous default gets a synthesized internal name).
  - a per-page `ModuleLoader` (`src/js/module.rs`) keys namespaces by
    absolute URL, resolves dependencies depth-first in document order and
    runs each body in its own child environment, so module bindings never
    leak into the page. Relative specifiers resolve against the importing
    module's URL; `export *` skips `default`; a failed module is dropped
    from the cache instead of being reused half-filled.
  - `nomodule` scripts are skipped when the document has a module build, so
    dual-build pages no longer run both halves.
  - `import()` / `import.meta` were deliberately kept as *runtime*
    failures: a parse error would kill every statement before them in the
    script (that is exactly what svelte.dev's bundle does).
  Honest shortcuts: exported bindings are a snapshot taken when the
  defining module finishes (no live bindings), a cyclic import sees an
  empty namespace, bare package specifiers do not resolve, dynamic
  `import()` / `import.meta` are unimplemented, and engine B logs + skips
  module scripts.
  Tests (614 default / 618 with `engine-b woff2`):
  `exports_values_default_and_functions`,
  `imports_default_named_and_namespace`, `module_scope_is_private`,
  `reexports_named_and_star`, `export_list_of_locals`,
  `export_default_anonymous_function`, `export_default_anonymous_class`,
  `failed_module_is_not_cached`, `module_script_runs_and_mutates_dom`,
  `module_imports_a_data_url_dependency`,
  `nomodule_fallback_is_skipped_when_a_module_exists`,
  `import_in_a_classic_script_errors_honestly`,
  `import_export_need_a_module_script`,
  `dynamic_import_fails_at_runtime_not_parse_time`,
  `import_dot_meta_does_not_stop_parsing`.
  Live: a `data:` page imports three data-URL modules (named, default,
  namespace) - 5/5 `ok`; a local HTTP page with relative imports across
  three files (named + default + `export *`) - 4/4 `ok`. MDN
  `/Web/CSS/list-style-type` still lays out at 2773 runs (it now discovers
  5 module bundles; each one fails on private class fields, the next syntax
  batch). Wikipedia `JavaScript` unchanged at 11404 runs; svelte.dev
  unchanged at 291 runs.
  **(v) bundle syntax** - the four MDN client bundles wave 15 downloaded now
  parse end to end:
  - `for` heads are parsed as a head: the declarator list is a list
    (`for (var a, b, c = x; ...)` no longer stops at the first comma when
    the first declarator has no initializer), and `Parser::no_in` keeps `in`
    out of the binary operators until the head ends (cleared inside
    parentheses), so `for (x in o)`, `for (o[k] in o)`, `for (n = f(), i =
    0; ...)` and `for (var x = ('a' in o); ...)` all parse.
  - class bodies accept `async` methods (and `async *`), generator methods
    (`*g()`), computed member names (`[Symbol.toPrimitive](v) {}`, static
    and instance fields) and private names (`#x`, `#m()`, `this.#m`).
  - tagged templates are real calls: `` tag`a${x}b` `` passes a strings
    array (with `.raw`) followed by the interpolated values.
  - the template scanner consumes a whole escape (`\xHH`, `\uHHHH`,
    `\u{...}`, line continuations) instead of exactly two bytes; a Fluent
    template containing `\xa9` used to fail with a bogus "bad \x escape"
    reported at the template's start.
  - `<script type>` is honored: only JavaScript MIME types and `module`
    execute; JSON-LD, import maps and template blocks are skipped
    (Wikipedia's inline `application/ld+json` block had been parsed as JS).
  Honest shortcuts: computed accessors (`get [k]()`), class static blocks,
  `#x in obj` brand checks and private-name scoping across a base/derived
  pair are not implemented; private names are ordinary `#`-prefixed
  properties; a computed instance field re-evaluates its key per instance;
  `.raw` holds the cooked text (escape spelling is not preserved); engine B
  still logs + skips module scripts.
  Tests (624 default / 628 with `engine-b woff2`):
  `for_head_declarator_list_without_initializer_on_the_first`,
  `for_head_comma_expression_init`, `for_in_with_a_member_target`,
  `in_suppression_stops_at_parentheses`,
  `async_and_generator_class_methods`, `computed_class_member_names`,
  `private_class_fields_and_methods`, `template_hex_escape_is_decoded`,
  `tagged_template_gets_strings_and_values`,
  `collect_scripts_skips_non_javascript_types`.
  Live: all four MDN bundles parse; MDN `/Web/CSS/list-style-type` now runs
  its module tree (2773 runs, 89.4-95.6 MB; the first executed bundle stops
  on `Cannot read property 'prototype' of undefined` - the next runtime
  gap), Wikipedia `JavaScript` 11404 runs / 92.1 MB, svelte.dev 291 runs /
  83.7 MB.
  **(w) URL + Object.prototype** - the web-platform objects MDN's bundles
  assume, plus error text that names the right file:
  - `URL` and `URLSearchParams` are real prototype-based natives over the
    `url` crate (`src/js/url.rs`): `href`, `protocol`, `host`, `hostname`,
    `port`, `pathname`, `search`, `hash`, `origin`, `username`, `password`,
    `searchParams`, `toString`/`toJSON`; `URLSearchParams` has `get`,
    `getAll`, `has`, `set`, `append`, `delete`, `sort`, `forEach`, `entries`,
    `keys`, `values`, `toString`, and accepts a query string, an array of
    pairs or a plain object. `new URL('.', someUrlInstance)` uses the base's
    `href` (a URL stringifies to it), which was a hard error on svelte.dev.
  - a native constructor can own `.prototype` (engine side table
    `native_props`), so a bundle's `o.U.prototype = URL.prototype` lines work
    and `instanceof` walks the registered prototype chain.
  - `Object.prototype` exists, `Object.getPrototypeOf` returns it, and plain
    objects and arrays inherit it lazily - `({}).hasOwnProperty('k')` and
    `Object.prototype.hasOwnProperty.call(o, 'k')` both work.
    `Object.create(proto)` and `Object.getOwnPropertyNames` added.
  - `location` exposes `protocol`, `host`, `hostname`, `port`, `pathname`,
    `search`, `hash`, `origin`.
  - runtime errors now name the position in the file that *threw*, not in the
    caller: every parsed source is registered by id (`register_source`,
    bounded to 256 sources / 4 MB per page) and each `FnDef` stamps its
    source id, which `call` swaps into `cur_src`. The innermost
    `eval_expr`/`eval_stmt` records the throw's origin once (`note_error`);
    only the escaping error's log text gains `(line L, col C)`, so a caught
    value is still exactly what `throw` passed. `(in f <- g)` frames stay.
  Honest shortcuts: `URL` components are snapshots (no setters) and
  `searchParams` is not live-linked to `href`; `URL.createObjectURL` is
  absent; `Object.create(null)` still inherits `Object.prototype` lazily; no
  `Object.defineProperties`/`freeze`/`seal`; string concat of an object uses
  the engine's own text rather than a script-defined `toString`; a throw
  from a span-less internal path (destructuring/spread helpers) reports the
  last evaluated expression's position; sources past the retention cap report
  the error without a position.
  Tests (641 default / 645 with `engine-b woff2`): `url_components`,
  `url_resolves_against_a_base`, `url_rejects_bad_input_and_requires_new`,
  `url_search_params`, `url_search_params_mutate_and_serialize`,
  `url_search_params_from_object_and_arrays`,
  `url_accepts_a_url_instance_as_base`,
  `object_prototype_is_reachable_and_inherited`,
  `object_create_and_property_names`,
  `escaping_error_names_the_source_that_threw`,
  `caught_errors_keep_their_value`,
  `a_caught_error_does_not_block_the_next_location`,
  `syntax_errors_report_their_own_position`.
  Live: MDN `/Web/CSS/list-style-type` 79.8-91.2 MB and its content renders
  (the two earlier `o.U.prototype=URL.prototype` and `hasOwnProperty of
  undefined` bundle errors are gone; one module still stops on `Cannot read
  property 'length' of undefined` - a lit/telemetry path, next gap); MDN
  reports that throw's position in the right bundle file (previously the
  caller's); svelte.dev 79.3-83.6 MB (the `new URL('.', urlObject)` error is
  gone; remaining inline errors are a canvas-2d context - `getContext`
  returns undefined - and one `parentElement` read); Wikipedia `JavaScript`
  101.6 MB / 11404 runs (identical).
  **(x) next** - BigInt literals, `new.target`, `for await`, top-level
  `await`, import assertions, live module bindings, computed class
  accessors, class static blocks.
  **(y) live element surface (style / classList / currentScript)** - the
  convenience API the svelte.dev inline scripts assume:
  - `element.style` is a host-backed CSSStyleDeclaration stand-in
    (`ObjKind::Style(u32)`): member reads/writes forward through two fn
    pointers the embedder installs on the engine (`style_get` / `style_set`),
    so `js.rs` stays DOM-free. `runtime.rs` owns the text handling:
    `style_text`, `style_prop`, `style_set` over the `style` attribute, with
    a quote/paren-aware declaration splitter and a general camelCase →
    kebab-case name rule (no per-property or per-vendor tables). Property
    writes (`el.style.width = "100px"`, `Object.assign(el.style, {...})`),
    `setProperty`, `getPropertyValue`, `removeProperty`, and `cssText`
    (read/replace) all reach the cascade; an empty value removes that
    declaration and an empty attribute is dropped.
  - `element.classList` (`add`/`remove`/`toggle`/`contains`) reads and
    rewrites the live `class` attribute; `element.parentElement` is a lazy
    accessor (wrapping a node never wraps its whole ancestor chain);
    `document.currentScript` names the `<script>` whose turn is running and
    is `null` outside script turns; `append` (element arguments) and
    `remove` complete the create/append/measure/remove cycle; the layout
    reads (`offsetWidth/Height`, `clientWidth/Height`) are 0 because this
    pipeline runs scripts before layout.
  Honest shortcuts: `style.length` and other numeric CSSOM reads come back
  as strings; `classList.value/length/replace` are absent; layout reads are
  0 rather than forcing a synchronous layout; `append` does not create text
  nodes from string arguments; the engine-core adapter (Option B) still
  exposes the wave-17 element surface (feature is compile-gated only).
  Tests (649 default / 653 with `engine-b woff2`):
  `style_object_forwards_writes_to_the_attribute`,
  `object_assign_onto_style_reaches_the_attribute`,
  `style_removal_and_css_text_replacement`,
  `class_list_mutates_the_live_class_attribute`,
  `current_script_and_parent_element_are_live`,
  `element_remove_detaches_from_parent`,
  `css_property_names_normalize`,
  `declarations_survive_quotes_and_functions`.
  Live: svelte.dev 78.7-82.9 MB / 291 runs - both wave-18 inline-script
  errors are gone; the only remaining inline error is the documented
  `import()` gap (HTML line 10 of the boot script). MDN
  `/Web/CSS/list-style-type` 87.5 MB / 2773 runs with the same pre-existing
  bundle `length` error; Wikipedia `JavaScript` 96.2-96.8 MB / 11404 runs
  with the same `match` error - no RAM or layout regression (97.0 MB
  baseline on the same URL).
  **(z) dynamic `import()` + `import.meta.url`** - the svelte.dev boot
  blocker, and the module-URL plumbing it exposed:
  - the global `import(spec)` native returns a real Promise and queues a
    `Job::Import` microtask. The engine's microtask pump takes the module
    registry out of the engine (re-entrantly), fetches/executes the module,
    then settles the promise - so `.then`/`.catch` chains and
    `await import(...)` both work, and a module body runs with the caller's
    frame depth restored (`await` pumps the load, so the outer call stack
    must survive it).
  - the specifier base is the URL of the module whose code is running: a
    `src_id -> URL` table lives on the engine, and a call frame reports its
    closure's source (`cur_src`), so a module-defined function called later
    from classic script code still resolves `./x.js` against its own module
    rather than the page.
  - `import.meta.url` builds `{url}` at read time from the same lookup (no
    `resolve()`); import assertions still do not parse; an unresolvable
    specifier rejects the promise (and logs once - there is no
    `unhandledrejection` event).
  Honest shortcuts: module jobs ride the microtask queue instead of the
  spec's separate module-job queue (ordering observable only against
  timers), a failed import is logged but never reported to the page as an
  `unhandledrejection` event, and `import.meta` has only `url`.
  Tests (656 default / 660 with `engine-b woff2`):
  `dynamic_import_resolves_from_page_base`,
  `dynamic_import_rejects_unresolvable_specifier`,
  `dynamic_import_specifier_is_module_relative`,
  `import_meta_url_names_the_defining_module`,
  `dynamic_import_inside_module_function_uses_module_base`,
  `dynamic_import_resolves_and_then_runs`,
  `await_dynamic_import_resumes_inside_a_call`.
  Live: svelte.dev 83.2 MB / 291 runs - no inline-script error remains; the
  22802-byte boot script now loads `entry/start...js`. Calling its `init`
  then stops in Vite's preload helper, which needs
  `document.getElementsByTagName`, `document.head` and a `link` element
  from `createElement` - the next DOM batch, not a module gap. MDN
  `/Web/CSS/list-style-type` 87.3 MB / 2773 runs (same pre-existing bundle
  error); Wikipedia `JavaScript` 96.9 MB / 11404 runs with the same
  `match` error (99.4 MB on the first run, the known load-time noise) - no
  RAM or layout regression.
  **(aa) DOM surface + modern JS gaps (svelte.dev boot chain)** - the Vite
  preload helper and the Svelte entry chunk asked for a batch of standard
  library surface; svelte.dev now boots all the way through and paints:
  - DOM: `document.getElementsByTagName` (one `by_tag` walk in the host,
    `"*"` = every element), `document.head` (synthesizes a `<head>` when the
    parsed tree has none), `createElement` interns any tag on demand
    (`Dom::tag_id` is now public), and a 15-entry reflected-attribute table
    installs live `id`/`className`/`href`/`src`/`rel`/`type`/`name`/`value`/
    `title`/`alt`/`width`/`height` accessors on every wrapper, so
    `link.rel = "stylesheet"` reaches the attribute.
  - `Object`: `fromEntries` (arrays of `[k, v]` pairs), `freeze`/`seal`
    (identity), `is` (SameValue), `isFrozen`/`isSealed`, `hasOwn`,
    `getOwnPropertyDescriptor(s)` with a real `get` slot for accessor
    properties — enough for `Object.getOwnPropertyDescriptor(
    ArrayBuffer.prototype, "byteLength").get`.
  - Collections: `Set`/`Map` plus `WeakMap`/`WeakSet`, all prototype-based
    with a hidden own payload (`__set`/`__map`) and a `size` accessor, so
    `instanceof` and `for…of` work through the prototype chain;
    `for_of_items` iterates the hidden payloads.
  - `Array.prototype.sort` (default lexicographic, stable insertion sort
    with a comparator) and `reverse`.
  - `Symbol` builtin (`iterator`/`asyncIterator`/`toStringTag`/
    `hasInstance` statics); `typeof` returns `"symbol"` and the value
    stringifies as `Symbol(desc)`.
  - `TextEncoder`/`TextDecoder` (UTF-8 round trip), `atob`/`btoa`, a
    `Uint8Array` stand-in (`toBase64`, `fromBase64`, `buffer`,
    `byteLength`), `Buffer.from(...).toString("base64")`, and
    `ArrayBuffer`/`SharedArrayBuffer` shells whose `byteLength` is a real
    accessor.
  - Parser: `async function*` (statement and expression), object-literal
    `async m(){}` / `async *m(){}` / `*m(){}`, `for await` (accepted as
    plain `for…of`), and `in` on function objects. The generator walker's
    `unreachable!` yield positions are now honest call-time TypeErrors.
  Honest shortcuts: symbols are unique objects with a hidden `__symbol`
  prop rather than a primitive (usable as computed keys via their
  `Symbol(desc)` text); typed arrays are plain arrays of byte values with
  no element-type coercion or ArrayBuffer backing; `Object.freeze`/`seal`
  do not record integrity levels; collection methods return arrays instead
  of iterator objects and their hidden payload is enumerable;
  `Object.fromEntries` accepts only array pairs; async generators run on
  the existing blocking `await` walker, so an unsupported yield position
  fails at call time instead of parse time; `for await` is `for…of`.
  Tests (674 default / 678 with `engine-b woff2`):
  `document_get_elements_by_tag_name_returns_descendants`,
  `document_head_is_synthesized_when_missing`,
  `reflected_properties_write_attributes`,
  `object_from_entries_builds_objects`,
  `object_from_entries_rejects_non_pairs`,
  `set_builtin_dedupes_and_iterates`,
  `map_builtin_holds_key_value_pairs`,
  `weak_collections_store_by_identity`,
  `symbol_builtin_is_unique_and_usable_as_a_key`,
  `array_sort_and_reverse`,
  `arraybuffer_byte_length_accessor`,
  `object_integrity_and_descriptor_helpers`,
  `text_encoder_decoder_round_trip`,
  `base64_and_typed_array_stand_ins`,
  `async_and_generator_object_method_shorthand`,
  `async_generator_functions_parse_and_suspend_like_generators`,
  `async_generator_parsing_and_for_await`.
  Live: svelte.dev 92.8 MB / 291 runs with a clean render (logo, hero
  conveyor image, hero copy, GET STARTED, footer) and no `[js]` failure;
  MDN `/Web/JavaScript/.../Array` 89.8 MB / 7776 runs and Wikipedia
  `/wiki/Web_browser` 92.4 MB / 5507 runs both keep their pre-existing
  bundle errors and their layouts.
  **(z) next** - BigInt literals, `new.target`, `for await`, top-level
  `await`, import assertions, live module bindings, computed class
  accessors, class static blocks, canvas 2d context, and the Vite preload
  DOM surface (`getElementsByTagName`, `head`, `createElement("link")`,
  attribute property setters).
- [ ] **Phase 3** — tabs + **hibernation: restore from serialized DOM+layout on disk, not
  re-fetch** (Recommendations §2), cookies, forms, memory-pressure discarding policy
  — DOM half done in wave 9 (o): snapshots serialize the parsed tree and restore
  skips fetch+parse; cookies done in wave 11 (q): a persisted jar sends on every
  request and `document.getCookie`/`setCookie` expose the script subset; forms /
  hibernation-on-pressure / a real tab store are open.
- [ ] Later — real CSS cascade/invalidation, damage tracking, fuzzing, sandbox study

## Recommendations logged (from "translate HTML/CSS to Rust" discussion)

1. **Stay runtime-interpreted** for compatibility — compiling arbitrary pages to Rust
   source can't work (dynamic content, rustc latency, binary bloat, ecosystem loss).
   Compile *artifacts*, not pages:
2. **CSS selectors precompiled once per stylesheet** into a compact rule tree
   (no raw selector re-matching per element) — DONE in Phase 1b (`css.rs::parse`).
3. **Hibernated tabs serialize parsed DOM + layout state to disk**; restore =
   deserialize, skip fetch+parse — Phase 3.
4. **Tag interning** — element tags stored as `TagId(u16)` into an interned table instead
   of per-node `String`s — DONE in v1.1.

## Measurements log

| Date | Build | Page | RSS |
|---|---|---|---|
| 2026-10-27 | Phase 0 | Wikipedia | 38.5 MB |
| 2026-10-27 | Phase 1 | Wikipedia | 80.0 MB (flat; fonts identified as cause) |
| 2026-10-27 | Phase 1 + tag interning | Wikipedia | 79.8 MB (interning = churn win, not baseline) |
| 2026-10-27 | Phase 2: lazy fonts + ab_glyph | Wikipedia | **24.1 MB** |
| 2026-10-27 | Phase 2 | example.com | 19.1 MB |
| 2026-10-27 | Phase 2: link click → navigate → back | Wikipedia (Main_Page) | 20.3 MB |
| 2026-10-27 | Phase 2: charset + CJK font fallback | GBK test page | 26.9 MB |
| 2026-10-27 | Phase 2: with fallback fonts (legit CJK+Hangul on page) | Wikipedia | 38.2 MB |
| 2026-10-27 | Phase 1b: CSS subset | CSS test page | 16.7 MB |
| 2026-10-27 | Phase 1b: CSS subset (75 rules from real sheet) | Wikipedia | 38.3 MB |
| 2026-10-27 | Phase 1b+: external `<link>` CSS (4 sheets, 260 rules) | nextjs.org | 18.7 MB |
| 2026-10-27 | Phase 2: HTTP disk cache (3 hits, cold RSS) | Wikipedia | 35.7 MB |
| 2026-10-27 | Phase 1c: box tree + flex/grid | nextjs.org | **16.9 MB** (was 19.2 MB; box tree = 356 nodes vs 1061 DOM nodes) |
| 2026-10-27 | Phase 1d: box chrome (pad/margin/lh/radius/bg) | nextjs.org | 15.7 MB (572 runs) |
| 2026-10-29 | Phase 1e: `<img>` PNG+JPEG (probe + display-size decode) | nextjs.org | **24.8 MB** (1 image decoded; naive full-decode was 50.0 MB) |
| 2026-10-29 | Phase 1e: local image test page (3 PNGs) | 127.0.0.1 | 19.0 MB |
| 2026-10-29 | Phase 1f: SVG rasterizer (8/9 images incl. logos) | nextjs.org | 45.1 MB (8 images at display size) |
| 2026-10-29 | Phase 1g: borders (strips/ring paint) | nextjs.org | 45.2 MB |
| 2026-10-29 | Phase 1h: position relative/absolute/fixed | nextjs.org | 48.2 MB (572 runs; positioned nav/footer) |
| 2026-10-29 | Phase 1i: box-shadow | nextjs.org | 45.6 MB (572 runs; 8 images) |
| 2026-10-29 | Phase 1j: font-family stacks (serif) | nextjs.org/docs (redirect) | 78.2 MB (2698 runs; docs page, much larger than homepage) |
| 2026-10-29 | Phase 1k: flex-wrap | nextjs.org | 45.9 MB (572 runs) |
| 2026-10-29 | Phase 1l: var() custom properties + hsl() | nextjs.org | 45.3 MB (572 runs; pill + themed colors render) |
| 2026-10-29 | Phase 1m: inline `<svg>` | nextjs.org | 41.9 MB (572 runs; 24 images — hero NEXT + icons) |
| 2026-10-29 | selector split + min-height fixes | nextjs.org | 42.0 MB (570 runs; hero NEXT floats above pill, footer no longer piled at top-left) |
| 2026-10-29 | UA-white + cross-site fixes (inline-block/margin, clamp/vw, grid calc+named col, flex-wrap) | rust-lang.org | 27.2 MB (517 runs; full nav + yellow button) |
| 2026-10-29 | UA-white + cross-site fixes | Wikipedia (Main_Page) | 34.4 MB (1326 runs; heading size fixed) |
| 2026-10-29 | font-size replace-against-parent cascade | Wikipedia (Main_Page) | 34.5 MB (1326 runs; h1 ~32px, full featured page) |
| 2026-10-29 | font-size replace-against-parent cascade | nextjs.org | 43.2 MB (570 runs; dark, no regression) |
| 2026-10-30 | color-scheme + light-dark + @supports | MDN | 26.3 MB (623 runs; dark = reference) |
| 2026-10-30 | color-scheme + light-dark + @supports (incl. scope_vars root-def fix) | Wikipedia (Main_Page) | 34.7 MB (1326 runs; fully light again) |
| 2026-10-30 | font stacks + @font-face webfonts | rust-lang.org | 28.6 MB (517 runs; hero = Alfa Slab One via WOFF1, body Fira Sans; +44 KB webfonts) |
| 2026-10-30 | font stacks + @font-face webfonts | hackernews | 12.9 MB (786 runs; Verdana via system alias) |
| 2026-10-30 | font stacks + @font-face webfonts | Wikipedia (Main_Page) | 34.6 MB (1326 runs; pixel-unchanged) |
| 2026-10-30 | form-control chrome | rust-lang.org | 14.0 MB (483 runs; select = one line, options never laid out → CJK font not pulled) |
| 2026-10-30 | inline-block sits on the text line | nextjs.org | 43.0 MB (571 runs; no regression) |
| 2026-10-30 | inline-block sits on the text line | rust-lang.org | 14.8 MB (483 runs; pixel-unchanged, same 2 webfonts) |
| 2026-10-30 | min-content/max-content/auto grid tracks | Wikipedia (Main_Page) | 35.5 MB (1461 runs; sidebar in proper column, +135 runs from content tracks sizing to content) |
| 2026-10-30 | min-content/max-content/auto grid tracks | nextjs.org | 43.0 MB (571 runs; byte-identical — no content tracks there) |
| 2026-10-30 | SVG class-based fill (inline-SVG logo color) | MDN | 14.8 MB (630 runs; logo now blue letters + light wordmark, was solid black) |
| 2026-10-30 | SVG class-based fill | rust-lang.org | 17.6 MB (483 runs; pixel-unchanged — gear logo not single-class-colored) |
| 2026-10-30 | SVG class-based fill | nextjs.org | 43.0 MB (571 runs; byte-identical) |
| 2026-10-30 | calc() arithmetic in lengths | MDN | 14.8 MB (630 runs; a11y skip-link menu now off-screen via `top: calc(-20em)`) |
| 2026-10-30 | calc() arithmetic in lengths | nextjs.org | 43.1 MB (571 runs; pixel-unchanged) |
| 2026-10-30 | calc() arithmetic in lengths | Wikipedia (Main_Page) | 35.1 MB (1461 runs; pixel-unchanged) |
| 2026-10-30 | eframe/glow shell swap (minifb gone) | example.com | 106.2 MB after layout (idle ~124 MB — eframe+winit+glow GL stack = ~+100 MB floor over minifb's ~15-20 MB) |
| 2026-10-30 | eframe/glow shell swap | Wikipedia (Main_Page) | ~158 MB idle (page engine still frees per-page state on nav — RSS drops after leaving heavy pages) |
| 2026-10-30 | P1 parity wave (pseudo-class selectors + DSD expansion + inline bg/br) | example.com | 47.9 MB (25 runs) |
| 2026-10-30 | P1 parity wave | hackernews | 49.7 MB (760 runs; titles black, no underlines) |
| 2026-10-30 | P1 parity wave | rust-lang.org | 57.6 MB (483 runs; yellow button full-extent) |
| 2026-10-30 | P1 parity wave | Wikipedia (Main_Page) | 73.5 MB (1458 runs; serif h1, no underline-everywhere) |
| 2026-10-30 | P1 parity wave | MDN | 66.2 MB (657 runs; DSD header restored, hero one line-gap) |
| 2026-10-30 | P1 parity wave | nextjs.org | 80.5 MB (502 runs) |
| 2026-10-31 | forms + scripts wave | Wikipedia (Main_Page) | 93.1 MB (1269 runs; + worker keeps DOM+JS engine+CSS between jobs, dropped on nav) |
| 2026-10-31 | event-loop wave | Wikipedia (Main_Page) | 100.8 MB (timers/microtasks/rAF queues + listener maps; engine still dropped per nav) |
| 2026-10-31 | language-depth wave | Wikipedia (Main_Page) | 97.1 MB (1255 runs; 70KB startup bundle now runs end-to-end) |
| 2026-10-31 | selectable runtime: A vs B | Wikipedia Rust article / DuckDuckGo ?q= | A 94.7 / 82.2 MB; B 96.3 / 176.2 MB (B heap lazy on simple pages; heavy script bundles cost far more than the +30 ballpark) |
| 2026-10-31 | gap-fix wave (lexer `$` spin fix + JS step budget) | MDN /Web/CSS/margin | 86.4 MB (2877 runs; first successful load — previously hung forever in the lexer, 57s CPU/60s wall; now 1.7s CPU/60s wall) |
| 2026-11-01 | wave 1 (box-sizing + overflow/clip culling) | Wikipedia Main Page | 96.4 MB (1253 runs), CPU 0.9s; skip-link "Jump to content" no longer renders (closed the documented clip/overflow known-gap) |
| 2026-11-01 | wave 2 (float/clear/vertical-align + background-size/position/repeat) | Wikipedia Main Page | 105.4 MB (1253 runs), CPU 0.5s; float text-wrap + right-floated sidebar + sized bg headers live (+9 MB vs wave 1 from tiled paint entries) |
| 2026-11-02 | wave 3 (::before/::after + content, z-index + transform) | Wikipedia Main Page | 97.5 MB (1468 runs, +215 pseudo boxes; RAM down 7.9 MB vs wave 2), CPU 0.8s |
| 2026-11-02 | waves 4+5 (mask-image; :hover + cursor; position:sticky; typography batch) | Wikipedia Main Page | 93.6 MB (1618 runs; +150 runs of preserved whitespace/typography, RAM down 3.9 MB vs wave 3 — no per-frame hover state at rest) |
| 2026-11-02 | waves 4+5 | Wikipedia Rust article | 102.7 MB (14740 runs; word-level runs on a long content page, 315 images decoded at display size) |
| 2026-11-02 | wave 6 (list markers, WebP, WOFF2) | Wikipedia Main Page | 106.9 MB (1646 runs; +13.3 MB = the WOFF2 webfonts the UI copy advertises; site serif active, 25 images decoded, 0 format skips) |
| 2026-11-02 | wave 6 | MDN `/Web/CSS/list-style-type` | 101.5 MB (4687 runs; 177 images, 0 format skips, markers render) |
| 2026-11-02 | wave 6 | local HTTP WebP page | 72.8 MB (12 runs; VP8X+ALPH decoded at 320x240, alpha visible) |
| 2026-11-02 | wave 7 (table layout, object-fit/aspect-ratio, image sources) | Wikipedia Rust article | 112.2 MB (14973 runs; infobox renders as a real two-column table, 315 images, 0 format skips; +9.5 MB vs wave 5 = WOFF2 site fonts) |
| 2026-11-02 | wave 7 | data: URL page | 68.8 MB (8 runs; PNG + WebP data images decoded inline) |
| 2026-11-02 | wave 8 (grid tracks, CSS math, scrollbar) | Wikipedia Rust article | 105.0 MB (14973 runs; same layout as wave 7, 315 images, 0 format skips) |
| 2026-11-02 | wave 8 | MDN `/Web/CSS/list-style-type` | 88.7 MB (4687 runs; `display:contents` + named grid areas give the real 3-column layout; outer box-shadow hole culls the gray band under sidebar/TOC; remaining: column widths, search widget stacking) |
| 2026-11-02 | wave 8f (media math, full `:is()`, logical box props, of-type, min-content) | MDN `/Web/CSS/list-style-type` | 83.2 MB (2943 runs; the `<1072px` media gate finally applies, so the wide sidebar is hidden — 1744 fewer runs; 704px baseline card, clean TOC/breadcrumb links, side padding, title chip, search button in its track; remaining: variant custom-property colors, search-pill wrap) |
| 2026-11-02 | wave 8g (flex-grow, nth-of-type family) | Wikipedia Rust article | 103.9 MB (14973 runs, 315 images; `flex-grow:1` puts the page heading at the title bar's left edge on one line) |
| 2026-11-02 | wave 8h (relayout image memo) | Wikipedia Rust article | 98.7 MB first layout (14973 runs); relayouts after it report `315 drawn (0 decoded, 315 from the memo)` - the 104 → 118 MB climb was that decode churn |
| 2026-11-03 | wave 8l (`<button>` container) | MDN `/Web/CSS/list-style-type` | 91.0 MB (2773 runs, 11 images - the search button's inline `<svg>` magnifier now lays out and paints) |
| 2026-11-03 | wave 8l (`<button>` container) | Wikipedia Rust article | 106.7 MB (14973 runs, 315 images; pixel-unchanged) |
| 2026-11-03 | wave 8m (`clip-path: inset()`) | data: URL `sr-only` probe | 54.7 MB (4 runs; both hidden labels clipped away, both visible strings paint) |
| 2026-11-03 | wave 8m (`clip-path: inset()`) | MDN `/Web/CSS/list-style-type` | 76.1–80.0 MB (2773 runs, 11 images; unchanged) |
| 2026-11-03 | wave 8n (sub/sup + line-through/overline) | data: URL decoration probe | 54.7 MB (28 runs; H₂O, mc², struck and overlined runs all paint) |
| 2026-11-03 | wave 8n (sub/sup + line-through/overline) | Wikipedia Rust article | 96.3-116.1 MB (14973 runs, 315 images; layout identical, `[n]` markers raised) |
| 2026-11-03 | wave 9 (DOM snapshots + restore) | Wikipedia Rust article (fresh load, snapshot saved 1347 KB) | 109.4 MB (14973 runs, 315 images; worker 11.8 s) |
| 2026-11-03 | wave 9 (DOM snapshots + restore) | Wikipedia Rust article (restored, no fetch) | 105.4-114.9 MB (19239 nodes, 14973 runs, 315 images; worker 4.1 s) |
| 2026-11-03 | wave 9 (DOM snapshots + restore) | example.com (restored) | 62.9 MB (25 runs; worker 43 ms vs 684 ms fresh) |
| 2026-11-03 | wave 10 (flex model completion) | Wikipedia Rust article | 107.3 MB (14973 runs, 315 images; pixel-unchanged) |
| 2026-11-03 | wave 10 (flex model completion) | MDN `/Web/CSS/list-style-type` | 81.9 MB (2773 runs, 11 images; unchanged) |
| 2026-11-03 | wave 10 (flex model completion) | rust-lang.org | 82.3 MB (483 runs; GET STARTED label inside its button, nav unchanged) |
| 2026-11-03 | wave 10 (flex model completion) | Hacker News | 73.8 MB (784 runs) |
| 2026-11-03 | wave 10 (flex model completion) | nextjs.org | 99.5 MB (502 runs) |
| 2026-11-03 | wave 10 (flex model completion) | example.com | 66.7 MB (25 runs) |
| 2026-11-03 | wave 11 (cookie jar) | local two-load probe | 63.4 → 63.9 MB (load 2 sent `Cookie: srv=1; js=2`; page printed both from the request and from `document.getCookie()`) |
| 2026-11-03 | wave 11 (cookie jar) | Wikipedia `/wiki/Web_cookie` (fresh) | 97.6 MB (12860 runs; 6 cookies stored, sent on every subresource) |
| 2026-11-03 | wave 11 (cookie jar) | Wikipedia `/wiki/HTTP_cookie` (new process) | 93.9 MB (12855 runs; the 6 cookies came from the persisted jar) |
| 2026-11-03 | wave 11 (cookie jar) | MDN `/Web/CSS/list-style-type` (restored) | 81.6 MB (2773 runs; unchanged) |
| 2026-11-03 | wave 12 (JS syntax batch 1) | `data:` 16-construct probe | 75.3 MB (16/16 `ok`: `?.`, `?.[]`, `?.()`, `[k]: v`, get/set, `**`, `**=`) |
| 2026-11-03 | wave 12 (JS syntax batch 1) | MDN `/Web/CSS/list-style-type` | 81.8 MB (2773 runs; unchanged — no regression) |
| 2026-11-03 | wave 12 (JS syntax batch 1) | Wikipedia `/wiki/JavaScript` (new URL) | 100.8 MB (11404 runs) |
| 2026-11-03 | wave 13 (class completion) | `data:` 11-class probe | 74.9 MB (11/11 `ok`: `extends`, `super()`, `super.m()`, static method/field/accessor/inheritance, instance fields, get/set pair, default ctor) |
| 2026-11-03 | wave 13 (class completion) | MDN `/Web/CSS/list-style-type` | 81.9 MB (2773 runs; unchanged — no regression) |
| 2026-11-03 | wave 13 (class completion) | Wikipedia `/wiki/JavaScript` | 11404 runs (identical; status-bar RSS 95.0 MB) |
| 2026-11-03 | wave 13 (class completion) | svelte.dev | 77.0 MB (577 nodes; client-rendered shell stays blank — its inline scripts fail on `catch {}` optional binding, a later syntax batch) |
| 2026-11-03 | wave 14 (JS syntax batch 2) | `data:` 13-construct probe | 75.6 MB (8/8 newly supported print `ok`: `catch {}`, `??=`, `||=`, `&&=`, labels, `break`/`continue label`, class expression, class expression + extends) |
| 2026-11-03 | wave 14 (JS syntax batch 2) | svelte.dev | 115.9–126.6 MB (291 runs; client-rendered hero now paints — `catch {}` was the blocker, plus an `Object.assign(fn, …)` worker panic fixed) |
| 2026-11-03 | wave 14 (JS syntax batch 2) | MDN `/Web/CSS/list-style-type` | 82.0 MB (2773 runs; unchanged — no regression) |
| 2026-11-03 | wave 14 (JS syntax batch 2) | Wikipedia `/wiki/JavaScript` | 100.1 MB (11404 runs; identical) |
| 2026-11-03 | wave 15 (ES modules) | `data:` 5-check module probe | 73.2 MB (5/5 `ok`: named import, default import, `import * as`, namespace `default`, `nomodule` half skipped) |
| 2026-11-03 | wave 15 (ES modules) | local HTTP ESM page (3 modules, relative imports) | 64.7 MB (2 runs; named + default + `export *` resolve over real HTTP) |
| 2026-11-03 | wave 15 (ES modules) | MDN `/Web/CSS/list-style-type` | 85.7 MB (2773 runs; layout unchanged; 5 module bundles now fetched but fail on private class fields) |
| 2026-11-03 | wave 15 (ES modules) | Wikipedia `/wiki/JavaScript` | 98.4 MB (11404 runs; identical) |
| 2026-11-03 | wave 15 (ES modules) | svelte.dev | 111.2 MB (291 runs; unchanged; dynamic `import()` is now a runtime error, so earlier statements still run) |
| 2026-11-03 | wave 16 (bundle syntax) | parse probe: the 4 MDN client bundles | 4/4 parse (was 0/4: `for` declarator list, `for (e in c)`, `async` method, computed class key, private `#m`, tagged template, template `\xa9`) |
| 2026-11-03 | wave 16 (bundle syntax) | MDN `/Web/CSS/list-style-type` | 89.4-95.6 MB (2773 runs; module bundles now run instead of failing to parse; first bundle stops on `Cannot read property 'prototype' of undefined`) |
| 2026-11-03 | wave 16 (bundle syntax) | Wikipedia `/wiki/JavaScript` | 92.1 MB (11404 runs; inline JSON-LD block no longer parsed as JS) |
| 2026-11-03 | wave 16 (bundle syntax) | svelte.dev | 83.7 MB (291 runs; unchanged rendering) |
| 2026-11-03 | wave 17 (URL + Object.prototype) | MDN `/Web/CSS/list-style-type` | 79.8-91.2 MB (2773 runs; `o.U.prototype=URL.prototype` and `hasOwnProperty of undefined` bundle errors gone, page content renders) |
| 2026-11-03 | wave 17 (URL + Object.prototype) | svelte.dev | 79.3-83.6 MB (291 runs; `new URL('.', urlObject)` fixed; remaining inline errors are a missing canvas-2d context and one `parentElement` read) |
| 2026-11-03 | wave 17 (URL + Object.prototype) | runtime-error locations | parse probe: a thrower in script A called from script B now reports line 2 of A (was: the caller's source, or no position) |
| 2026-11-03 | wave 17 (URL + Object.prototype) | Wikipedia `/wiki/JavaScript` | 101.6 MB (11404 runs; identical; one inline script stops on `Cannot read property 'match' of undefined`) |
| 2026-11-03 | wave 18 (style/classList/currentScript) | svelte.dev | 78.7-82.9 MB (291 runs; both inline-script errors fixed; only dynamic `import()` remains) |
| 2026-11-03 | wave 18 (style/classList/currentScript) | MDN `/Web/CSS/list-style-type` | 87.5 MB (2773 runs; same pre-existing bundle `length` error; rendering unchanged) |
| 2026-11-03 | wave 18 (style/classList/currentScript) | Wikipedia `/wiki/JavaScript` | 96.2-96.8 MB (11404 runs; 97.0 MB baseline on the old binary; same `match` error, no regression) |
| 2026-11-03 | wave 19 (dynamic import) | svelte.dev | 83.2 MB (291 runs; the boot script's `import()` loads the entry module — no inline-script error left; `init` stops in Vite's preload helper, the next DOM gap) |
| 2026-11-03 | wave 19 (dynamic import) | MDN `/Web/CSS/list-style-type` | 87.3 MB (2773 runs; same pre-existing bundle `length` error) |
| 2026-11-03 | wave 19 (dynamic import) | Wikipedia `/wiki/JavaScript` | 96.9 MB (11404 runs; 99.4 MB on the first run, known load-time noise; same `match` error) |
| 2026-11-03 | wave 19 (dynamic import) | local ESM probe (2 modules, nested + awaited imports) | 65.2 MB (1 run; `await import()` resumes inside an async arrow, module-relative specifiers resolve, the reaction's DOM write paints) |
| 2026-11-03 | wave 20 (DOM surface + stdlib) | svelte.dev | 92.8 MB (291 runs; full render — logo, hero image, hero copy, GET STARTED, footer — and no `[js]` failure) |
| 2026-11-03 | wave 20 (DOM surface + stdlib) | MDN `/Web/JavaScript/.../Array` | 89.8 MB (7776 runs; same pre-existing bundle `length` error, layout unchanged) |
| 2026-11-03 | wave 20 (DOM surface + stdlib) | Wikipedia `/wiki/Web_browser` | 92.4 MB (5507 runs; same pre-existing inline `match` error, layout unchanged) |
| 2026-11-03 | wave 21 (prototypes + CSS4 colour + grid) | MDN homepage | 96.3 MB (615 runs; hero no longer one word per line - 505px copy column inside an 872px header, featured-article cards in two columns; same pre-existing bundle `length` error) |
| 2026-11-03 | wave 21 (prototypes + CSS4 colour + grid) | react.dev | 158.3 MB (1534 runs; dark theme, hero copy and buttons; Next router still logs `parsePath`) |
| 2026-11-03 | wave 21 (prototypes + CSS4 colour + grid) | svelte.dev | 127.4 MB (291 runs; pixel-unchanged) |
| 2026-11-03 | wave 21 (prototypes + CSS4 colour + grid) | Wikipedia `/wiki/Main_Page` | 103.2 MB (1433 runs; unchanged, same 2 script errors) |
| 2026-11-03 | wave 22 (flex/float measure + document.cookie) | react.dev | 150.6 MB (1534 runs; 167.4 MB on an earlier run — this page swings ~10 MB; hero h1 78px centred, buttons inline; no router error left) |
| 2026-11-03 | wave 22 (flex/float measure + document.cookie) | svelte.dev | 91.9 MB (291 runs; pixel-unchanged) |
| 2026-11-03 | wave 22 (flex/float measure + document.cookie) | MDN homepage | 98.5 MB (615 runs; unchanged; same single bundle `length` error) |
| 2026-11-03 | wave 22 (flex/float measure + document.cookie) | Wikipedia `/wiki/Main_Page` | 105.7 MB (1434 runs; **zero `[js]` errors**; search toggle inside its box, no overlap with the personal links) |
| 2026-11-03 | wave 25 (box-sizing heights) | Wikipedia `/wiki/Main_Page` | 112.2 MB (1434 runs, 1347 boxes; restored snapshot; every 44px icon button now measures **44 tall** with its 1px border, previously 46 — `MAZDA_DUMP_BOXES=2` shows `label...cdx-button… h=44` and `a...search-toggle h=44`) |
| 2026-11-03 | wave 25 (box-sizing heights) | MDN `/Web/CSS/list-style-type` | 88.7 MB (2773 runs; layout unchanged, same pre-existing bundle `length` error) |
| 2026-11-03 | wave 27 (SVG clipPath/mask/filter/pattern/use) | in-crate SVG + tokenizer probe | n/a (no live page; 17 SVG tests - clip/mask/pattern/use/symbol/filter pixels, mask fill inheritance, nested viewport - plus the new tokenizer test and the whole suite: default **707**, `engine-b + woff2` **711**) |
| 2026-11-03 | wave 29 (JS gaps: error positions, real iterators, typed arrays, BigInt, `new.target`, private brand checks) | in-crate JS probe | n/a (no live page; whole suite **725** pass / 0 fail; new tests: BigInt arithmetic + conversions, typed-array aliasing/width coercion/ArrayBuffer views, `new.target` + `#x in o`, user `[Symbol.iterator]` laziness, caught-error and module-body error positions) |

### Wave 29 findings

- **MDN's `length-of-undefined` was not a missing property — it was the error
  *position*.** `err_located` latched for a whole execution turn, so a throw
  that a `try`/`catch` had already swallowed made the next escaping throw report
  the earlier statement's offset. Two more paths moved the diagnostic cursor
  without putting it back: loading a module body (`cur_src` stayed on the
  module's source) and resuming a generator (`.next()` ran with the *caller's*
  source). Fix: `eval_stmt` / module bodies reset the cursor per statement, and
  module loads + generator resumes save and restore `cur_src`/`err_located`.
  Regression tests: `caught_errors_do_not_shift_later_error_locations`,
  `module_body_error_locations_stay_per_statement`.
- **Iterators are real objects now** (`ObjKind::Iterator` + `IterState`), not
  arrays dressed up: `.next()` returns `{value, done}`, `it[Symbol.iterator]()
  === it`, `return()` closes. `for..of` is the lazy protocol (one `.next()` per
  element, so `break` stops pulling from an infinite generator); spread,
  `Array.from`, destructuring and the built-in constructors collect through the
  same path with a 100k cap (a runaway user iterator fails instead of hanging).
  User objects only need `[Symbol.iterator]`; arrays/strings/typed arrays/Set/
  Map are fast-pathed.
- **Set/Map `keys`/`values`/`entries` return iterator objects** and both
  prototypes carry `Symbol.iterator`. Set's `keys === values`; Map's `entries`
  yields `[k, v]`, `keys` the key and `values` the value. Tests that used to call
  `.join()` on the result now spread it.
- **Typed arrays are real views over real bytes.** Eleven constructors
  (`Int8Array` … `BigUint64Array`) share `%TypedArray%.prototype`; each view is
  `kind + byteOffset + length` over an `ArrayBuffer`'s `Rc<RefCell<Vec<u8>>>`,
  so `subarray` aliases, `slice` copies, index writes coerce to the element
  width (`a[0] = 300` on a `Uint8Array` stores 44), and `ArrayBuffer.isView`,
  `BYTES_PER_ELEMENT`, `buffer`, `byteLength`, `byteOffset` all report truth.
  `Buffer.from(str)` is a `Uint8Array` view with a base64-aware `toString`.
- **BigInt is `i128`, and says so.** Decimal/hex/oct/bin `n` literals, arithmetic
  (including `**`, `/`, `%`, bitwise), mixed BigInt comparisons, `typeof`,
  `toString(radix)`, `BigInt()`, `BigInt.asIntN`/`asUintN`. Mixing BigInt and
  Number throws an honest `TypeError`; values or results outside `i128` throw a
  `RangeError` rather than silently wrapping (documented range limit).
- **`new.target`** is parsed as its own expression and bound in every non-arrow
  function env, so arrows inherit it lexically (the `new.target === F` check
  inside an arrow in a constructor works). Plain calls see `undefined`.
- **`#x in obj`** evaluates as a has-property check on the private name. Private
  fields remain ordinary `#name` props, so this is not per-class mangling: two
  different classes declaring the same `#x` cannot tell their brands apart. The
  common bundler/`instanceof`-style brand check works; the corner case is
  recorded here rather than hidden.
- **Shortcut, typed arrays**: methods are `set`, `subarray`, `slice`, `join`,
  `toString`, `toBase64` only — no `map`/`filter`/`forEach`/`sort`/`indexOf`.
  `BigInt64Array`/`BigUint64Array` store through the same numeric conversion
  (i128 → f64) instead of a true 64-bit integer path.
- **Wave numbering note**: waves 27 (SVG paint pass) and 28 (TLS/media) landed
  first; this JS wave is 29.

### Wave 28 findings

- **TLS: tune rustls, not `wreq`** (decision record above, in the crate policy).
  `src/net.rs` is the single network-identity module: Chrome UA + client hints +
  `Accept-Language` per request, and a rustls provider whose cipher suites and
  key-exchange groups are sorted into Chrome's order. Asserted by 5 unit tests.
- **One identity, not two.** `navigator.userAgent` used to say `Mazda/1.0` while
  the socket said something else — a one-line fingerprint. Both now read
  `net::USER_AGENT`.
- **Honest headers beat a long header list.** `Accept-Encoding` advertises
  `gzip` only (ureq's default decoder), and `Accept` omits `image/avif` (we
  cannot decode AVIF). Advertising `br`/`avif` would trade a fingerprint tell
  for broken resources.
- **ALPN stays `http/1.1`** because ureq is HTTP/1.1-only. Claiming `h2` would
  break every request; the JA4 delta is accepted and recorded.
- **EME fail-fast, DRM still parked.** `navigator.requestMediaKeySystemAccess`
  and `mediaCapabilities.decodingInfo` exist and reject with a real
  `NotSupportedError` promise (settled on the microtask queue, so `.catch` runs
  instead of the caller throwing). Absent methods made players show the generic
  "unsupported browser" wall.
- **Media elements say no truthfully.** `canPlayType()` returns `""`, `play()`
  rejects with `NotSupportedError`, `pause()`/`load()` are inert — on the shared
  element prototype, so non-media elements are unaffected.
- **`<video>`/`<audio>` render a placeholder, not a hole.** UA sizes (video
  300×150, audio 300×32), `poster` through the existing image pipeline (so a
  poster frame still paints), otherwise a `[media] playback not supported`
  leaf. Engine rule re-learned: block boxes take height from content, so the UA
  size lands as `min_height`; and this engine's inline flow sizes a box from its
  text child, so media is boxed as block (spec says inline-level — recorded as a
  shortcut in the code).
- **Engine B** gets the same contract: UA constant, EME/mediaCapabilities
  bootstrap eval, and `canPlayType`/`play`/`pause`/`load` on the shared
  prototype (`play` rejects with a real rejected promise).

### Wave 27 findings

- **The SVG paint pass is now a recursive element walk, not a streaming
  matrix/style stack.** `use`, `clipPath` content, `mask` content, `pattern`
  content and filter inputs all have to re-enter arbitrary subtrees; parallel
  stacks cannot express that. `next_tag` now records each tag's `<` offset
  (`Tag.start`) and every container is painted by `paint_range(start, end)`.
- **Definitions are harvested once by `id`** (`harvest_defs`) into byte ranges
  (tag name, attrs, content start/end), so any element — not only `defs`
  children — can be referenced. Missing close tags run to EOF; recursion is
  depth-capped (24 levels of containers, 16 for `use`) so a cyclic sprite
  cannot hang a page.
- **Nested `<svg>` gets its own viewport**: `x`/`y`/`width`/`height` +
  `viewBox` (xMidYMid meet) compose into the parent's matrix, percentages
  resolve against the parent viewport, and the viewport rectangle is pushed as
  a clip. `<use>` of a `symbol`/`svg` applies the same transform and
  width/height defaults.
- **`clip-path`** builds coverage from the referenced `clipPath`'s shapes
  (nested groups and `<use>` included), honours `clipPathUnits =
  objectBoundingBox` and the element's own transform. Coverage is ANDed for
  nested clips.
- **`mask`** coverage is luminance × fill alpha × `fill-opacity`, the default
  mask type; the `<mask>` element's own `fill` inherits into its shapes (so
  `<mask fill="white">` works without repeating the fill on every child),
  `maskContentUnits` is honoured, and the default `-10%..120%` mask region
  clips stray content.
- **`pattern`** honours `patternUnits`, `patternTransform` and
  `objectBoundingBox` units. Content is painted once in absolute device
  coordinates and the result is tiled by modular sampling of the first tile.
- **Filters** run on an isolated ARGB layer per element, then clip/mask apply
  to the filter result (per spec). Implemented primitives: `feGaussianBlur`
  (3-pass box approximation, per-axis radius), `feOffset`, `feFlood`,
  `feColorMatrix` (`matrix`, `saturate`, `hueRotate`, `luminanceToAlpha`),
  `feMerge`, `feBlend`, `feComposite` (`over`/`in`/`out`/`atop`/`xor`/
  `arithmetic`) and `feDropShadow`. Named `result` chains and
  `SourceGraphic`/`SourceAlpha` inputs work.
- **`use`** clones shapes, groups and viewport roots (`symbol`/`svg`) with
  `x`/`y` offsets; a definition is never painted in place, only where it is
  referenced. `foreignObject` is skipped rather than painted as SVG.
- **Nested-`<svg>` tokenizer truncation fixed.** `<svg>` is RAW_TEXT, so the
  tokenizer stopped at the first `</svg>`; an inline sprite
  (`<svg><symbol>…</symbol></svg><svg>…`) lost everything after the inner
  close. `html::raw_text_end` now depth-counts `<svg>` (other raw-text
  elements cannot nest and keep the old first-close behaviour). Regression:
  `nested_svg_raw_text_keeps_inner_markup` asserts both the inner close and
  the markup after it survive in the payload, and that the document continues
  after the outer `</svg>`.
- **Shortcuts taken**: gradients still collapse to the mid-stop colour
  (pre-existing); pattern content is painted once and folded into the first
  tile (content that intentionally overflows its tile is clipped); the filter
  region is a flat 10% bbox pad rather than `filterUnits`/`x`/`y`/`width`/
  `height`; `mask-type: alpha` is not read (luminance is assumed); no
  `feImage`, `feTurbulence`, `feMorphology`, `feComponentTransfer` or
  `feTile`; `clip-rule` is always nonzero; `marker` and `text` remain skipped.
- **Verification note**: the full suite ran in a clean worktree at `HEAD`
  with only `src/svg.rs` + `src/html.rs` overlaid — concurrent sessions had
  `src/js/*` mid-edit in the shared tree, so those changes were deliberately
  not pulled in. Default **707** tests, `engine-b + woff2` **711**, all pass.

### Wave 25 findings

- **`height`/`min-height` now follow `box-sizing` like `width`.** The engine's
  height model was content-box while widths were border-box, so a
  `min-height: 44px` touch target with a 1px border rendered 46 tall. A new
  `as_content_h()` helper converts a declared height to the content height
  `place()` stores: border-box (the engine default) subtracts the vertical
  padding+border once, `box-sizing: content-box` keeps the declaration as
  content height. `min-height`, replaced `<img>` heights, aspect-ratio heights
  and grid percent-row available height all route through it, so a 44px button
  is 44 tall and a `width:100px; aspect-ratio:2` box is 100×50 outer.
- **UA control heights keep their old outer size.** `control_chrome`'s
  content-box minimums were restated in border-box terms: checkbox/radio
  12 → 14 (still a 14×14 outer box), text inputs/buttons/submits/selects
  20 → 22 (still one 20px line plus the 2px UA border).
- Tests: default **703** on the wave-25 base (**725** with waves 27-29 applied),
  `engine-b + woff2` **707** on the base; five new guards
  (`min_height_floors_the_border_box_by_default`,
  `min_height_content_box_floors_the_content`,
  `declared_height_follows_box_sizing`, `aspect_ratio_follows_box_sizing`,
  `grid_percent_rows_resolve_against_the_content_height`), plus the mask paint
  snapshot updated to the new border-box `min-height:60px` geometry.
- Known gaps: declared `height` on a plain block is still ignored (only
  replaced/aspect-ratio/grid boxes consult it); `%` `min-height` still resolves
  against width, not the container height; `%` vertical padding uses the same
  width-basis shortcut as before.

### Wave 22 findings

- **`width: auto` must reset an earlier `width`.** The shorthand cascade kept
  the old `decl::WIDTH`/`bx.width`, so `w-full sm:w-auto` stayed full width.
  react.dev's hero buttons now size to their labels instead of spanning the row.
- **Column-flex items with `align-self` ≠ stretch are shrink-to-fit and their
  MARGIN box is centred** (previously `place()` got the full width and only the
  box was shifted, so a centred `h1` spanned the page).
- **`+` adjacent-sibling combinator** implemented (`Seg.adj`); Wikipedia's
  `.cdx-button span + span` rule now hides the duplicated button labels.
- **`inline-flex`/`inline-grid`** keep their inner flex/grid layout but sit on
  the surrounding line (`BoxStyle.inline_box`) instead of opening a block line.
- **Flex/grid items are block formatting contexts** (`BoxNode.bfc`): floats
  inside them are contained and `block()` grows the box to the lowest float.
  `overflow` ≠ visible already behaved that way and now shares the flag.
- **Flex-row `min-height` centres items vertically** (`align-items: center/
  end`), so icon glyphs sit in the middle of their 44px buttons.
- **`natural_w` now clamps by `min-width`/`max-width`.** A float's prefer-width
  comes from the intrinsic measure; Wikipedia's search toggle is an empty span
  with a 44px touch-target minimum, so it measured 6px, the float:right sat 37px
  past its box and overlapped "Donate"/"Create account". Fixing the measure
  fixes both the width (44) and the position (right-aligned with the search box).
- **`document.cookie` is a real accessor** (the getter/setter pair already
  existed as `getCookie`/`setCookie`). Reading it returned `undefined`, so
  Wikipedia's client-preference bootstrap threw `Cannot read property 'match' of
  undefined` and its whole inline script died — the page actions row lost the
  bookmark icon and the skin classes stayed at the no-JS defaults.
- **`String.prototype.lastIndexOf` / `Array.prototype.lastIndexOf`** added; the
  mediawiki startup loader calls `modules[i].lastIndexOf('.')` and threw
  `undefined is not a function` (line 14, col 682), which stopped every module
  bundle on every Wikipedia page. `/w/load.php` now runs clean and the page logs
  **no script errors at all**.
- **Known gaps after wave 22**: MDN's client bundle still throws `Cannot read
  property 'length' of undefined` (reported column lands inside a CSS string, so
  the position mapping, not the read, is suspect); SVG `<text>`/`textPath` are
  not painted (MDN's mandala); Chrome-side geometry shows the Vector 2022 menu
  landmark is 20px wide while its 44px button overflows it — ours reserves 44,
  so the logo sits 24px further right; no clip/mask/filter/pattern/`use`; no real
  `symbol` primitive, BigInt, `new.target`, class static blocks, `#x in obj`,
  typed arrays, or live module bindings; `Set`/`Map` iterators are emulated by
  core-js.

### Wave 21 findings

- **`ToString` on an object was walking its properties** (exponential on a
  cyclic graph such as `window`). gtag.js evaluates `window == null`, which
  coerced the global object and ballooned to gigabytes before the page rendered.
  `ToString`/`to_primitive` now follow the spec tag (`[object Object]`,
  `[object Function]`); `inspect()` kept only for `console.log`.
- **Contextual `of`**: only the for-head separator is special; `var of = 3` and
  `of(4)` are plain identifiers again. `no_in` suppression resets inside parens
  and object/array literals so `'a' in o` survives any for-init.
- **Prototype surface for feature probes**: primitive prototypes (String,
  Number, Boolean, Symbol, Function), `Function.prototype.call/apply/bind/
  toString`, `Object.prototype.propertyIsEnumerable/valueOf/isPrototypeOf`,
  `Object.defineProperties`, `Error.prototype.toString` + per-class prototypes
  + `.stack`, `Promise.prototype` (`then`/`catch`/`finally`),
  `RegExp.prototype`, well-known `Symbol.*`, `Array.of/from`,
  `String.prototype.charCodeAt`, `Array.prototype.splice`, and callable
  `Object`/`Array` (hidden `__call` hook).
- **The 112KB core-js bundle now runs with zero errors.** Root cause of the last
  failure was `Promise.prototype` not existing: `pm = Promise.prototype` gave
  `undefined`, so the polyfill's own `then` install threw. A real prototype
  object plus promise instances chained to it fixed it.
- **react.dev now loads with no chunk errors**: `window.history`
  (push/replaceState, back/forward/go, `scrollRestoration`, `state`,
  `length`), `navigator.platform`/`language`, `MediaQueryList.addEventListener`
  (no-ops), and the Error family above were the missing pieces.
- **CSS Color 4 syntax** (`rgb(35 39 47 / var(--tw-text-opacity))`,
  `hsl(... / 0.5)`, percentage channels) now parses. react.dev's hero text was
  white because the colour declaration had been dropped as invalid, not because
  of a cascade bug.
- **Grid fidelity**:
  - `grid-column-end` and the `a / b` shorthand now span named lines (start
    resolves through `-start`, end through `-end`); only `grid-column-start`
    used to be read.
  - `justify-content: normal` = stretch: leftover space is shared by `auto`
    tracks.
  - Tracks are "maximized" with an equal share of the free space (freezing at
    each growth limit) instead of greedily feeding the first track.
  - A grid item whose overflow is not visible has an automatic minimum of 0,
    so a clipped decorative panel cannot starve the text column beside it.
  On MDN's homepage these four turn a 177px one-word-per-line hero into the
  reference layout (505px copy in an 872px header; reference 533/877) and put
  the featured-article cards in two columns.
- **Known gaps after wave 21**: MDN's client bundle still throws `Cannot read
  property 'length' of undefined` (index.js col 19277); Wikipedia still throws
  inline `match` of undefined + `load.php` `undefined is not a function`; Next.js
  client routing stops on `parsePath` of undefined (caught, logged bare); SVG
  `<text>`/`textPath` are not painted (MDN's mandala); the Vector 2022 header
  search/menu boxes still overlap the logo; no real `symbol` primitive, BigInt,
  `new.target`, class static blocks, `#x in obj`, typed arrays, or live module
  bindings; `Set`/`Map` iterators are emulated by core-js.

### Phase 1c (Track A) findings

- **Box tree beats flat atoms on RAM *and* fidelity**: 356 box nodes for
  nextjs.org (vs the old per-text-chunk atom list); hero is centered via
  `text-align`/flex justify, feature cards stack as real block boxes instead of
  one run-length line each. Height 1825 → 2284px because block children now
  occupy full-width rows (closer to a desktop browser's flow).
- **Anonymous item merging matters**: consecutive text leaves merge into one
  flex/grid item (spec behavior) — testing two `leaf()`s as separate items was
  the top source of test bugs; real DOM always separates items with elements.
- **Paragraph-gap semantics survived the rewrite**: the legacy `size * 0.6`
  gap at block boundaries is driven by `BoxNode.open` (last line has content),
  reproducing old `hard_break` layout without the atom-stream insertions.
- Latent bug found by live page (fixed): attribute-selector parse indexed past
  the buffer on an unterminated `[attr=` selector.

### Phase 1b findings

- **CSS costs ~0 RAM**: selectors precompiled once per stylesheet into a sorted
  rule list (specificity, then source order) — matching per element is a linear
  scan of `Rule.segs` (right-to-left, ancestor walk). Wikipedia: 16.5KB sheet →
  75 rules, RSS 38.2 → 38.3 MB.
- **Tokenizer raw-text bug (found by visual test)**: `<style>`/`<script>` content
  was *skipped entirely* (no text token, no end tag) — style elements got no text
  child AND every element after a body `<script>` nested inside it. Fixed: raw text
  is now emitted as a text child + synthesized end tag. Layout's `NON_RENDERED`
  list keeps script/style/title/head off-screen.
- Supported: type/class/id/compound selectors, descendant + `>` combinators, comma
  lists, attr selectors `[a=v]`/`[a]`, specificity+order cascade, inline `style=""`,
  `display:none`, page `background-color` (body/html only), color/font-size/weight/
  style/family/text-decoration. Colors: `#rgb`/`#rrggbb`/`#rrggbbaa`/`rgb()`/names.
- Skipped (parse-time dropped): pseudo-classes, `@media`/`@import` (brace-skipped),
  `!important`, shorthand `font:`. Underlines carry the
  run's color (was hardcoded link-blue).
- **External `<link rel=stylesheet>`** now fetched (document order, interleaved
  with `<style>`); rel parsed space-separated so `rel="preload stylesheet"` counts
  and `rel="icon"` doesn't. Wikipedia: 242KB → 327 rules; nextjs.org: 4 sheets →
  260 rules, 18.7 MB, full content + links render.
- **Known CSS gaps found on real sites**: `var(--x)` colors unsupported (nextjs.org
  bg falls back to our default dark), no `@media` (mobile-first sheets apply as-is),
  utility-class-heavy pages style only where the class rules survive our drops.
- **HTTP disk cache**: `%LOCALAPPDATA%\mazda-cache`, FNV-1a key, `<ts>\n<ct>\nbody`
  format, 5-min TTL (our own — no ETag/304 revalidation), oldest-mtime eviction at
  300 files, 5MB body cap. Wikipedia reload: HTML + 2 CSS = 3 hits, zero network.

### Phase 2 findings (root causes found & fixed)

- **fontdue parsed a 937KB font to ~15MB**; 4 fonts ≈ 46MB — the dominant cost.
  Replaced with **ab_glyph** (lazy parse, ~1MB raw bytes/font). Measured in-test:
  fontdue +14.8MB vs ab_glyph +0.0MB per font.
- Lazy `OnceLock` variant loading kept (italic/mono skipped on pages not using them).
- **Whitespace regression from Phase 1**: `"\n\t"` text chunks reached draw →
  `glyph_id('\n')` = GlyphId(0) = .notdef boxes on screen. Fixed: layout splits words on
  any whitespace (regression test `tabs_and_newlines_never_reach_draw`).
- Double paragraph gaps between consecutive hard breaks → page height 55k→38k px.
- Known gap: no font fallback yet — CJK/Thai/Arabic glyphs show as .notdef boxes
  (Segoe UI Latin only). Candidate for Phase 2b.
- **Links/clicking/history**: attrs parsed in tokenizer (quote-aware `>` scan), `href`
  carried through walk/layout as `Rc<str>` (resolved against base only on click),
  run-rect hit-test, Backspace = back.
- **Font fallback (2b)**: `pick_font_for(ch, style)` — primary style font, then
  `msyhl.ttc` (YaHei Light: han + kana, 11.6MB) for CJK ranges, then `malgunsl.ttf`
  (Hangul, 4.9MB). Range-gated (`is_cjk`/`is_hangul`) so scripts with no fallback
  font (Thai, Arabic…) never pay the load cost just to learn there's no glyph —
  a Thai-only page pays 0, while Wikipedia's 38.2 MB is legitimate (its sidebar
  genuinely shows 中 and 한 → both slots load, +16.5 MB). Fallback is regular-weight only (no faux bold).
  Verified coverage in-test: msyhl han=8066 kana=1204 hangul=0 thai=0, malgun hangul=11916.
- **Charset**: `into_string()` was UTF-8-lossy → legacy pages (GBK/Shift-JIS/1251) became
  `�`. Now: raw bytes → header charset → `<meta charset>` prescan (first 1024 bytes,
  both `charset=` forms) → UTF-8 fallback; BOM wins via encoding_rs. Verified live on a
  locally-served GBK page (bytes `E4B8AD...` in stderr = 中文测试页面).
  gzip needed no work: ureq's default `gzip` feature already negotiates (1MB → 158KB wire).
- **Known gaps**: navigation blocks the UI thread (Wikipedia reload ~15s, input queued
  during load), no cursor change over links, no `#fragment` scroll (same-doc links are
  no-ops), fetch capped at 20MB per page.

## Non-goals v1

GPU compositor, process sandbox, full CSS, media playback, extensions, DevTools.

JavaScript was originally a non-goal; **user decision 2026-10-30: Stage 1 JS support is
now in scope** (from-scratch tree-walking interpreter, no third-party engine code).
Roadmap: Stage 1 = interpreter core + `addEventListener`/DOM reads+writes/`fetch`/queue-
only `setTimeout` — enough for vanilla-JS interactive sites (Wikipedia gadgets, small
widgets, DuckDuckGo); RAM cost measured and logged at every step. Later stages (not
committed): ES2015+ depth (promises/microtasks, classes), incremental re-layout on DOM
mutation. Pixel-parity on heavyweight JS applications (google.com-class) explicitly NOT
a near-term target: those need a Chrome-scale API surface + JIT; recorded honestly here
so the goalposts don't drift.
**User decision 2026-10-31: selectable script runtime.** Option A = the pure-Rust
built-in interpreter (default, the engine completed in steps); Option B = a full
JS engine core selected from the settings page (compatibility mode, ~+30 MB per
page), compiled in behind a cargo feature flag so the default build unchanged.
The settings page presents exactly:
```
JavaScript engine:
  (•) A — Pure Rust built-in (light)
  ( ) B — Full JS engine core (compatibility, +30 MB/page)
```
Both sit behind one `ScriptRuntime` trait
sharing the same DOM natives in `script.rs`; a settings page in the browser UI
persists the choice (`mazda.conf` alongside the disk cache), default A. Sequenced
after the language-depth + event-loop waves land (same files). Per-app selection,
not per-tab (per-tab would double engine RAM — shortcut).

**User decision 2026-10-31: selectable font loading (WOFF2).** Reverses the
`a182f8f` "WOFF2 permanently unsupported" decision. Option A = WOFF2 webfonts
(default; site fonts render, ~+8-15 MB/page for a Wikipedia-class page);
Option B = system fonts only (minimal RAM, today's behavior). The settings page
presents exactly:
```
Font loading:
  (•) A — WOFF2 webfonts (site fonts, +~10-15 MB/page)
  ( ) B — System fonts only (minimal RAM)
```
Decoder behind cargo feature `woff2` (release built WITH it so A is selectable;
B costs nothing until chosen); `fonts=` in `mazda.conf`, `--fonts=` >
`MAZDA_FONTS` > conf > default A; applies on next navigation. A from-scratch
Brotli + WOFF2 transform decoder is ~4-6 waves; adopting the pure-Rust `wuff`
crate (see crate policy below) cuts it to ~1-2 integration waves. Per-page
decoded-font budget (LRU cap, e.g. 32 MB) so one CJK-heavy page can't blow the
RAM floor. `src/woff.rs` stays the container dispatch (`wOF2` → feature-gated
decoder, else honest fallback).

**User decision 2026-10-31: crate policy.** "From scratch" = the *engine*
(DOM, CSS cascade, layout, rasterizer, JS interpreter). General-purpose
utility codecs/font-decoders are adoptable — must be pure Rust (no FFI), permissively
licensed (MIT/Apache/BSD), well-download-counted, and must NOT be browser-engine
projects (keeps the no-third-party-engine-code rule and the git-scrub rule clean).
Approved for the next waves: `wuff` (WOFF2), `image-webp` (WebP), `brotli-decompressor`
(WOFF2 fallback path), `symphonia-format-isomp4` + `rusty_h264`/`rust_h265` +
`rusty-opus`/`rusty_mp3` (media tier, gated). Each adopted crate gets a RAM
measurement row; a crate that costs more than it saves gets dropped.

**Decision 2026-11-03 (wave 28): tune rustls, do NOT adopt `wreq`.** `wreq` was
provisionally approved above; on inspection it fails two clauses of this policy.
(1) It links `btls`/`btls-sys` = BoringSSL, C + FFI — the policy says pure Rust.
(2) It needs rustc 1.98 + edition 2024 (toolchain here is 1.96), and it replaces
the synchronous ureq worker with a tokio runtime, raising the per-page RAM floor.
The shipped alternative is `rustls` as a DIRECT dependency (already in the tree
via ureq, so no new build cost): Chrome-ordered cipher suites and curves, Chrome
request headers, and the UA the JS and network layers both report. The honest
limit is recorded in `src/net.rs`: rustls owns the ClientHello extension order
and never emits GREASE, so **JA3 cannot match Chrome**; JA4 ignores order/GREASE
but includes ALPN and ureq is HTTP/1.1-only, so it reports `h1` where Chrome
reports `h2`. Revisit only if a pure-Rust ClientHello + HTTP/2 stack appears.
No BoringSSL, not now or later, for a fingerprint.

**User decision 2026-10-31: DRM parked (not "impossible").** The honest reasons,
for the record: (1) an uncertified CDM gets refused or quality-capped by the
services server-side anyway (reimplementations like pywidevine/openwv exist on
GitHub but can't be certified); (2) shipping bypass/leaked-key tooling is legally
exposed (DMCA §1201-class); (3) it's ~5% of page parity. Deferred "later" per user.
Companion work that IS in scope: a cheap EME fail-fast surface so DRM-protected
sites show `[media] DRM not supported` instead of an "unsupported browser" wall.

**YouTube = recorded non-goal, with live numbers (2026-10-31).** Baseline ~12% of
reference-browser parity (video 40%/JS app 30%/shell CSS 15%/images 10%/interaction
5%). Ladder: +Wikipedia CSS tiers → ~25%; +WebP +TLS +EME → ~35%; +JS app boot
(20-40 waves, no crate helps) → ~65%; +media tier (revised to ~10-20 waves with
the approved crates: symphonia demux + rusty_h264 + audio + from-scratch
presentation) → ~95%; last ~5% = DRM (parked above). VP9/AV1: no pure-Rust decoder
exists — H.264 fallback quality tiers only. Same class as the google.com-class note
above: recorded so goalposts don't drift.

## Environment notes

- Toolchain: rustc 1.96.0, registry mirror rsproxy.cn (had corrupted sources once —
  fix = delete affected `registry\src` dirs and rebuild)
- `cargo build -j 1` used during flaky ICE episode; normal parallel builds since OK
