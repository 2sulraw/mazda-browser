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
  `Sheet::svg_decls` resolves last-in-cascade var()-substituted +
  `light-dark()`-picked values, and `build()` injects them into each open tag's
  inline `style` (which svg.rs prefers over presentation attributes). MDN's
  logo went from solid black to blue letters + light wordmark. Wave 35 closed
  the single-class shortcut: descendant/child chains, tag/id/attribute
  compounds and `font-size` (rem/em) resolve too, which is what draws the
  homepage mandala; SVGs no rule matches pass through byte-identical (verified
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
  `Sheet::pseudo` buckets so the origin element's `resolve`/SVG paint/
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
  column. Row-spanning items paint from their start row and their height is
  guaranteed by growing the last covered row (wave 34; before that a tall
  spanning sidebar could overhang its last row); numeric/`span` `grid-area`
  forms and named *lines* beyond `grid-column: <name>` are still not modeled.
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
  - DOM half done in wave 9 (o): snapshots serialize the parsed tree and restore
  skips fetch+parse; cookies done in wave 11 (q): a persisted jar sends on every
  request and `document.getCookie`/`setCookie` expose the script subset; the tab
  store landed in wave 30 (the tab list persists across restarts and restores
  through the same snapshot path); submission fidelity landed in wave 31
  (radio groups, disabled controls, `<select>` values, the activated submit
  button's pair); wave 32 added label->control clicks and the implicit
  submission's default-button rule. Still open: a file picker and a
  `<select>` popup.
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
| 2026-11-03 | wave 24 (intrinsic margin contributions) | Wikipedia `/wiki/Main_Page` | 103.1 MB (1347 boxes, 1434 runs; same run count as wave 22 — geometry only; menu landmark 20px wide, logo box at 64, `-12px` flush-left button at 12..56, all matching the reference) |
| 2026-11-03 | wave 24 (intrinsic margin contributions) | rust-lang.org | 89.2 MB (452 boxes, 483 runs; pixel-unchanged) |
| 2026-11-03 | wave 25 (box-sizing heights) | Wikipedia `/wiki/Main_Page` | 112.2 MB (1434 runs, 1347 boxes; restored snapshot; every 44px icon button now measures **44 tall** with its 1px border, previously 46 — `MAZDA_DUMP_BOXES=2` shows `label...cdx-button… h=44` and `a...search-toggle h=44`) |
| 2026-11-03 | wave 25 (box-sizing heights) | MDN `/Web/CSS/list-style-type` | 88.7 MB (2773 runs; layout unchanged, same pre-existing bundle `length` error) |
| 2026-11-03 | wave 27 (SVG clipPath/mask/filter/pattern/use) | in-crate SVG + tokenizer probe | n/a (no live page; 17 SVG tests - clip/mask/pattern/use/symbol/filter pixels, mask fill inheritance, nested viewport - plus the new tokenizer test and the whole suite: default **736**, `engine-b + woff2` **740** with all six waves merged) |
| 2026-11-03 | wave 29 (JS gaps: error positions, real iterators, typed arrays, BigInt, `new.target`, private brand checks) | in-crate JS probe | n/a (no live page; whole suite **736** pass / 0 fail after the other five waves merged; new tests: BigInt arithmetic + conversions, typed-array aliasing/width coercion/ArrayBuffer views, `new.target` + `#x in o`, user `[Symbol.iterator]` laziness, caught-error and module-body error positions) |
| 2026-11-03 | wave 30 (tab store) | Wikipedia `/wiki/JavaScript` | 100.1 / 101.9 MB (10943 runs, restored snapshot) against 99.6 / 100.0 MB from a HEAD build run back-to-back on the same URL - the delta is run noise; the store holds only the URL strings, the page itself is untouched |
| 2026-11-03 | wave 30 (tab store) | live restart probe | 2-tab `session` file -> two pills (second active) and the active tab taken from its DOM snapshot (`[dom] restored snapshot ... (no fetch)`); an explicit URL argument starts a fresh one-tab session; 14 s of idle frames left the file's mtime unchanged; suite **737** default / **741** `engine-b + woff2` |
| 2026-11-03 | wave 31 (form submission fidelity) | local `form31.html` probe (scripted `form.submit()`, no input synthesized) | `[js] form.submit() -> GET https://example.com/s?q=hi&lang=fr&region=Europe+wide` - the disabled input and the disabled `<fieldset>`'s control are gone, both `<select>`s submit (`selected` option value, label text with collapsed whitespace), suite **740** default / **744** `engine-b + woff2` |
| 2026-11-03 | wave 32 (label activation + implicit submission) | local `form31.html` probe with a label + checked box | `[js] form.submit() -> GET ...?agree=1&q=hi&lang=fr&region=Europe+wide` (still no submitter pair for a scripted submit - regression-free); label->control resolution covered by an in-crate test over `collect_elements`; suite **741** default / **745** `engine-b + woff2` |
| 2026-11-04 | wave 33 (grid span sizing + intrinsic whitespace) | MDN homepage, same build pair back-to-back (`MAZDA_DUMP_RUNS=2`) | the "Latest news" date moved from x=121 (on top of the source link) to x=398 in a card ending at 463 - right-aligned like the reference; the `max-content` column is the 58px date again instead of the 615px summary, `1fr` takes the rest; card/did-you-know grid re-wraps unchanged otherwise (2542px total both builds, 615 runs); Wikipedia `/wiki/JavaScript` 24365 -> 24454px because intrinsic text widths no longer count source indentation (a paragraph beside the infobox wraps one line earlier); suite **744** default / **748** `engine-b + woff2` |
| 2026-11-04 | wave 34 (grid row spans) | MDN homepage + Wikipedia `/wiki/JavaScript`, same build pair | a spanning grid item's height now stretches the rows it covers instead of overhanging them: MDN's hero grid grew 288 -> 420px because `div.homepage-header__mandala` (351x420) spans both header rows, so "Featured articles" moves down 132px (page 2542 -> 2675px, 615 runs); Wikipedia unchanged at 24454px (10943 runs) because no grid there has a row span; suite **745** default / **749** `engine-b + woff2` |
| 2026-11-05 | wave 35 (page CSS inside inline SVG) | MDN homepage + a local page with the real mandala markup and the page's rules | the mandala now paints (5 concentric rings of slashes, was blank): the page's `.mandala svg>text` and `.mandala textPath[href="#circleN"]` rules reach the rasterizer through a mini-cascade over the SVG's ancestor chain, and `font-size: 1.5rem` resolves in svg.rs; page height 2675px, 615 runs and ~100 MB unchanged, Wikipedia unchanged at 24454px; suite **746** default / **750** `engine-b + woff2` |
| 2026-11-05 | wave 36 (live ES-module namespaces) | MDN homepage + Wikipedia `/wiki/JavaScript` + nextjs.org | MDN's client bundle now runs to completion - the `Cannot read property 'length' of undefined` at `index.08e…js` col 19277 is gone and the page logs no `[js]` errors (page 2675px / 615 runs / 100.5 MB, one layout); Wikipedia unchanged at 24454px / 10943 runs / 101.6 MB; nextjs.org 7032px / 502 runs / 141.4 MB (fresh 256 KB page, unchanged by this wave); suite **748** default / **752** `engine-b + woff2` |
| 2026-11-05 | wave 37 (Function constructor compiles source) | Wikipedia `/wiki/JavaScript` + MDN homepage + nextjs.org + rust-lang.org + Hacker News | Wikipedia's 20 `Exception in resolve` / `Skipped unavailable module` pairs are gone (ResourceLoader evaluates each module's `skip` condition with `new Function(...)()`, which returned a plain object and threw) and the page logs no script errors; layout unchanged at 24454px / 10943 runs / 102.2 MB and the worker finished in 12.4s (wave 36: 18.2s); MDN 2675px / 615 runs / 99.7 MB, nextjs 7032 / 502 / 140.5 MB (same 11 pre-existing errors), rust-lang 483 runs, HN 734 runs (its `getElementsByClassName` gap is unrelated); suite **750** default / **754** `engine-b + woff2` |
| 2026-11-05 | wave 38 (`getElementsByClassName` + element `getElementsByTagName`) | Hacker News + Wikipedia `/wiki/JavaScript` + MDN homepage | HN's `hn.js` now runs clean (`byClass` calls `el.getElementsByClassName`): script errors 1 -> 0, geometry unchanged at 1695px / 734 runs / 78.1 MB; Wikipedia 24454px / 10943 runs / 103.0 MB and MDN 2675px / 615 runs / 100.0 MB carry no script errors; suite **751** default / **755** `engine-b + woff2` |
| 2026-11-05 | wave 39 (grid `justify-content`) | MDN homepage + nextjs.org + Wikipedia `/wiki/JavaScript` + Hacker News | grid columns now honour `justify-content` (`start`/`center`/`end`/`space-between`/`space-around`/`space-evenly`) and only `normal`/`stretch` grows `auto` tracks; flex no longer collapses `space-around`/`space-evenly` into `space-between`. No reference page changes: MDN 2675px / 615 runs / 100.0 MB, nextjs 7032px / 502 runs / 140.8 MB (same 11 errors), Wikipedia 24454px / 10943 runs / 103.2 MB, HN 1695px / 734 runs / 78.3 MB; suite **754** default / **758** `engine-b + woff2` |
| 2026-11-05 | wave 40 (JS syntax compatibility) | nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org | three lexer/parser gaps that rejected shipped framework code: regex `u`/`v`/`d` flags (the flag was lexed as an identifier), non-ASCII identifiers (lodash's deburr table uses accented keys), and computed keys in destructuring (`let {[key]: v, ...rest} = obj`). nextjs.org's script errors drop 11 -> 7 (all four syntax errors gone) with geometry unchanged at 7032px / 502 runs / 145.8 MB; MDN 2675px / 615 runs / 99.9 MB, Wikipedia 24454px / 10943 runs / 101.1 MB, HN 1695px / 734 runs / 77.9 MB, rust-lang 5800px / 483 runs / 84.8 MB, all error-free; suite **757** default / **761** `engine-b + woff2` |
| 2026-11-05 | wave 41 (`performance` clock + rAF timestamp) | nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org | `performance` was an absent global, so nextjs's streaming bootstrap (`$RT=performance.now()`) and every timing bundle threw. Added a monotonic `performance.now()` / `timeOrigin` plus no-op `mark`/`measure`/`getEntries*`, and rAF callbacks now receive the frame timestamp on the same clock. nextjs.org's remaining rAF error disappears (7 -> 6 script errors) with geometry unchanged at 7032px / 502 runs / 145.3 MB; MDN 2675px / 615 runs / 98.7 MB, Wikipedia 24454px / 10943 runs / 100.9 MB, HN 1695px / 734 runs / 78.0 MB, rust-lang 5800px / 483 runs / 84.9 MB, all error-free; suite **759** default / **763** `engine-b + woff2` |
| 2026-11-05 | wave 42 (DOM comments + node navigation) | nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org | React's streaming bootstrap (`$RX`/`$RC`/`$RV`) parks boundary markers in comments and walks/rewrites them, but the HTML parser dropped comments and wrappers had no sibling navigation. Comments are now real arena nodes (`nodeType` 8, no box); wrappers gained live `data`, `nodeType`, `parentNode`, `previousSibling`/`nextSibling`, `firstChild`, `removeChild`, `insertBefore` and a `dataset` object, and text nodes get the same wrapper. nextjs.org's five inline `.data` errors disappear (6 -> 1 script error) and the streamed boundary content now actually swaps in (7032px / 502 runs -> 7578px / 569 runs, 148.7 MB); Wikipedia 24454px / 10943 runs / 102.7 MB, HN 1695px / 728 runs / 78.2 MB, rust-lang 5800px / 483 runs / 84.9 MB stay error-free; MDN moves to 2922px / 641 runs / 88.6 MB only because bumping the snapshot format forced a fresh fetch of a newer page (the old cached DOM was stale); suite **761** default / **765** `engine-b + woff2` |
| 2026-11-05 | wave 43 (lazy prototypes in `getPrototypeOf`/`instanceof`) | nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org | Prototype links are attached lazily on first member access, but `Object.getPrototypeOf` read the raw slot and returned `null` for any fresh array/iterator/error. core-js's iterator helper then did `null.next` and threw. Added `JsEngine::proto_of` (explicit slot, else the shared prototype for the kind; module namespaces and `Object.prototype` itself are genuinely null) and routed `getPrototypeOf`, `instanceof`, `in`, `isPrototypeOf`, `chain_get` and setter lookup through it. nextjs.org's last script error disappears (1 -> 0) at unchanged 7578px / 569 runs; RAM settles ~162 MB because the app now boots fully and its router prefetches re-render repeatedly (first paint ~155 MB, bounded). MDN 2922px / 641 runs / 87.4 MB, Wikipedia 24454px / 10943 runs / 102.1 MB, HN 1695px / 728 runs / 78.1 MB, rust-lang 5800px / 483 runs / 84.8 MB, all error-free; suite **762** default / **766** `engine-b + woff2` |
| 2026-11-05 | wave 44 (property-name interning) | nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org | Every property insert allocated its own `Rc<str>` copy of the key, so a React render paid one string per slot for a handful of repeated names. A thread-local intern table (`KEY_INTERN`, cap 4096, cleared per `JsEngine::new`) now hands out one shared `Rc` per name; used by `obj_set_own`, `set_member`'s function-property insert and `Object.defineProperty`'s function path. Deterministic 150k-object x 8-key workload: 108.0 MB -> 71.7 MB, **36.3 MB (~34%) saved**. Live pages geometry-stable and error-free: nextjs 7578px / 569 runs / first layout ~152 MB, MDN 2922px / 641 runs / 87.3 MB, Wikipedia 24454px / 10943 runs / 101.1 MB, HN 1695px / 728 runs / 78.1 MB, rust-lang 5800px / 483 runs / 84.7 MB; suite **763** default / **767** `engine-b + woff2` |
| 2026-11-05 | wave 45 (document/Node surface + `queueMicrotask`) | nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org | Added the P1 document/Node members: `queueMicrotask` (a `Job::Call` in the existing FIFO microtask queue, non-callables throw), `document.forms` and `getElementsByName`, `createTextNode` / `createComment` (real text/comment arena nodes), and `Node.childNodes` / `Element.children` (document-order snapshots). No layout change; all five references are geometry- and error-identical: nextjs 7578px / 569 runs / first layout 150.2 MB (settles 153-158 MB as its router re-renders), MDN 2922px / 641 runs / 87.5 MB, Wikipedia 24454px / 10943 runs / 101.2 MB, HN 1695px / 728 runs / 78.0 MB, rust-lang 5800px / 483 runs / 84.5 MB; suite **768** default / **772** `engine-b + woff2` |
| 2026-11-05 | wave 46 (`DataView` + `AbortController` / `AbortSignal`) | nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org | Added the two remaining self-contained P1 constructors. `DataView` is a byte window over an `ArrayBuffer` with big-endian default, per-call little-endian flag, all 10 numeric accessor pairs (including BigInt), and RangeError/TypeError bounds checks. `AbortController` / `AbortSignal` expose `signal`, `aborted`, `reason`, `onabort`, `addEventListener` / `removeEventListener` for `abort`, `throwIfAborted`, and `AbortSignal.abort()`. No layout change; all five references keep their exact geometry and stay error-free: nextjs 7578px / 569 runs / 155.2 MB, MDN 2922px / 641 runs / 87.6 MB, Wikipedia 24454px / 10943 runs / 101.2 MB, HN 1695px / 728 runs / 78.4 MB, rust-lang 5800px / 483 runs / 84.9 MB; suite **773** default / **777** `engine-b + woff2` |
| 2026-11-05 | wave 47 (`Event` / `CustomEvent` + `dispatchEvent`) | nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org | Added the P1 event surface. `new Event(type, {bubbles, cancelable})` and `new CustomEvent(type, {detail, bubbles, cancelable})` build plain objects on linked prototypes (`CustomEvent` is an `Event`), with `preventDefault`, `stopPropagation` and `stopImmediatePropagation`. `element` / `document` / `window` `dispatchEvent` is synchronous: target, then ancestors, then the shared window/document bucket; `bubbles: false` stays on the target; `stopImmediatePropagation` also skips later listeners at the same stop; the return value is `!defaultPrevented`. No layout change; all five references keep their exact geometry and stay error-free: nextjs 7578px / 569 runs / 154.9 MB, MDN 2922px / 641 runs / 87.5 MB, Wikipedia 24454px / 10943 runs / 102.2 MB, HN 1695px / 728 runs / 78.0 MB, rust-lang 5800px / 483 runs / 84.9 MB; suite **778** default / **782** `engine-b + woff2` |
| 2026-11-05 | wave 48 (`MutationObserver`) | nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org | `new MutationObserver(callback)` with `observe` / `disconnect` / `takeRecords`. The shared DOM host logs `childList`, `attributes` and `characterData` mutations (plus `subtree`, `attributeOldValue` / `characterDataOldValue`, `attributeFilter`), but only while an observer is registered, so pages that never observe pay nothing. Callbacks run at the end of a script turn with `(records, observer)`; each record carries `type`, `target`, `addedNodes`, `removedNodes`, `attributeName` and `oldValue`. No layout change; all five references keep their exact geometry and stay error-free: nextjs 7578px / 569 runs / 155.6 MB, MDN 2922px / 641 runs / 87.7 MB, Wikipedia 24454px / 10943 runs / 99.7-102.6 MB, HN 1695px / 728 runs / 78.1 MB, rust-lang 5800px / 483 runs / 85.0 MB; suite **781** default / **785** `engine-b + woff2` |
| 2026-11-05 | wave 49 (`overflow-wrap` / `word-break`) | a local 120px-box wrapping page + nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org | Line layout can now break inside a word. `overflow-wrap: break-word|anywhere`, the legacy `word-wrap` alias and `word-break: break-all|break-word|keep-all` parse into two inherited `Style` switches; when a word still does not fit, the breaker cuts the longest character prefix that fits (`fit_prefix`, a binary search over monotone widths) and continues on the next line. A local page isolates the behavior: a 28-character token in a 120px box stays one 217px overflowing run by default, becomes 117px + 76px with `break-word`, 112px + 97px with `break-all`, and short words on one line are untouched. Four references keep their exact geometry and stay error-free: nextjs 7578px / 569 runs / 155.1 MB, MDN 2922px / 641 runs / 87.5 MB, HN 1695px / 728 runs / 78.0 MB, rust-lang 5800px / 483 runs / 84.6 MB. Wikipedia moves to 24575px / 10950 runs / 100.9 MB because its own `word-break: break-all` / `break-word` rules now take effect (intended fidelity gain, 7 extra run pieces); suite **785** default / **789** `engine-b + woff2` |
| 2026-11-05 | wave 50 (`text-overflow`) | a local 160px-row ellipsis page + nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org | Clipped lines can now end with an ellipsis. `text-overflow` is accepted and sets a box-level `ellipsis` flag (any `ellipsis` token matches, so the two-value form parses); `apply_run_clips` trims, per line, the run crossing the right content edge to the longest prefix that still leaves room for "…", empties later runs on that line, and leaves fitting lines and boxes without `overflow` alone. Runs are emptied, never removed, so other boxes' ranges stay valid. A local page shows the change directly: a 160px `nowrap`/`overflow:hidden` row dumps `ABCDEFGHIJKLMNOPQRST…` (153px) with `text-overflow: ellipsis`, while the same row without the declaration, a fitting label, and a `text-overflow`-only box without `overflow` keep their full text. No geometry change; all five references stay error-free: nextjs 7578px / 569 runs / 154.3 MB, MDN 2922px / 641 runs / 87.7 MB, Wikipedia 24575px / 10950 runs / 101.8 MB, HN 1695px / 728 runs / 78.1 MB, rust-lang 5800px / 483 runs / 84.8 MB; suite **787** default / **791** `engine-b + woff2` |
| 2026-11-05 | wave 51 (`max-height`) | a local capped/overflow page + nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org | A box can now cap its used height. `max-height` parses (`none`/`auto`/garbage clear it) and `place()` clamps the content height after layout, before the `min-height` floor, so taller content simply overflows — clipped only when `overflow` says so, and a larger `min-height` still wins (CSS Sizing 3). The clamp reuses the existing `box-sizing` rule: the declaration caps the border box by default and the content under `content-box`. A local page isolates it: with `max-height: 90px` a seven-line box measures 90px (four lines painted under `overflow: hidden`), the same box without `overflow` spills its text over the next box, a box whose content is shorter is unchanged, and `max-height: 40px; min-height: 120px` stays 120px. Four references are byte-identical; MDN improves: 2922 -> 2690px (runs 641 unchanged, 87.6 MB) because `.homepage-header__mandala { max-height: 20rem; overflow: hidden }` now holds the 35rem globe to 320px, which is what a real browser does. nextjs 7578px / 569 runs / 155.0 MB, Wikipedia 24575px / 10950 runs / 102.3 MB, HN 1695px / 728 runs / 78.2 MB, rust-lang 5800px / 483 runs / 84.8 MB; suite **792** default / **796** `engine-b + woff2` |
| 2026-11-05 | wave 52 (`outline`) | a local outline page + nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org | A box can now paint an outline: a ring outside the border box that layout never sees. `outline` and its `outline-width`/`-style`/`-color`/`-offset` longhands parse into a compact `BoxStyle.outline` (on, width, optional colour, offset); every non-`none` style paints solid, `auto` counts as off, an unset colour is currentcolor, and the width defaults to the CSS `medium` 3px. Paint emits four strips around the border box shifted outward by `outline-offset`, mapped through the box transform and culled by the inherited clip like the border strips. A local page shows all five cases: a 4px red ring hugging the border box, the same ring pushed 6px out by `outline-offset`, a `none` box with no ring, `thick dashed #06c` drawn as a 5px solid blue ring, and `1px solid transparent` keeping its geometry but painting nothing. All five references are geometry-identical and error-free: nextjs 7578px / 569 runs / 155.4 MB, MDN 2690px / 641 runs / 88.0 MB, Wikipedia 24575px / 10950 runs / 103.0 MB, HN 1695px / 728 runs / 79.3 MB, rust-lang 5800px / 483 runs / 85.8 MB; suite **796** default / **800** `engine-b + woff2` |
| 2026-11-05 | wave 53 (`transform-origin`) | a local five-origin page + nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org | The 2D transform composite can now scale about a declared point instead of always the box centre. `transform-origin` parses one or two values — `left`/`center`/`right`, `top`/`center`/`bottom`, lengths and percentages — into symbolic `Len`s on `BoxStyle.origin`; `%` resolves against the box's own border-box w/h at paint time, a lone vertical keyword claims the Y axis, keyword pairs may be written in either order, and `initial`/`unset`/`revert`/garbage fall back to `50% 50%`. `box_xf` now uses `p·s + (o(1−s) + d)` with `o` the resolved origin, so `scale()` pins the declared point and paint-only behavior is unchanged. A local page isolates it: five `scale(0.5)` boxes stay pinned at the centre, `0 0`, `100% 0`, `bottom right`, and `top left` respectively. All five references are geometry-identical and error-free: nextjs 7578px / 569 runs / 155.5 MB, MDN 2690px / 641 runs / 88.0 MB, Wikipedia 24575px / 10950 runs / 102.2 MB, HN 1695px / 728 runs / 78.1 MB, rust-lang 5800px / 483 runs / 85.3 MB; suite **798** default / **802** `engine-b + woff2` |

| 2026-11-05 | wave 54 (SVG intrinsic-size scaling) | a local three-size SVG page + nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org | Fixed a vector-rasterization bug: an SVG that declares `width`/`height` but no `viewBox` was rendered as if its user-space viewport were the requested raster size. Rendering the 144-unit rust-lang logo at 80px therefore mapped only user units 0–80 and clipped the gear to a corner fragment (144px happened to match, 200px showed it unscaled). `root_space` and `render` now derive the fallback viewport from the root `width`/`height` attributes (then the requested size), so the intrinsic 144-unit coordinate system is scaled to any display size; percentage lengths resolve against the same viewport. A local page embeds the same logo at 144px, 80px and 200px and now shows the full gear at all three. All five references stay geometry-identical and error-free: nextjs 7578px / 569 runs / 155.3 MB, MDN 2690px / 641 runs / 87.9 MB, Wikipedia 24575px / 10950 runs / 103.0 MB, HN 1695px / 728 runs / 78.4 MB, rust-lang 5800px / 483 runs / 84.7 MB — with the rust logo now whole in its capture; suite **799** default / **803** `engine-b + woff2` |

| 2026-11-05 | wave 55 (`text-decoration` paint controls) | a local decoration page + nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org | Underlines, strikethroughs and overlines can now be painted with their authored colour, pattern, thickness and offset. `text-decoration-color`, `text-decoration-style` (`solid`/`double`/`dotted`/`dashed`/`wavy`), `text-decoration-thickness` and `text-underline-offset` parse into four compact `Style` fields; the `text-decoration` shorthand also picks a style/colour/thickness token from its list, while `text-decoration-line` keeps changing only the line flags. Paint expands `double` into two rules inside the declared thickness, `dotted`/`dashed` into short solid segments (the wire channel is solid rects only), and moves only the underline by `text-underline-offset`; the run's own colour is the default. A local page shows a plain link, a red underline, a 4px red underline shifted 6px down, dotted and dashed 4px rules, a double rule, and wavy documented as solid. All five references keep their exact geometry and stay error-free: nextjs 7578px / 569 runs / 155.4 MB, MDN 2690px / 641 runs / 88.1 MB, Wikipedia 24575px / 10950 runs / 103.2 MB, HN 1695px / 728 runs / 78.3 MB, rust-lang 5800px / 483 runs / 85.2 MB — MDN's link decoration now carries its authored colour and offset; suite **801** default / **805** `engine-b + woff2` |
| 2026-11-05 | wave 56 (`align-content` / `place-content`) | a local five-grid + two-flex distribution page + nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org | Grid rows and wrapped flex lines can now be distributed in the container's spare block space. `align-content` and the `place-content` shorthand parse into one compact `Justify` field on `BoxStyle`; the layout keeps its existing single-pass paint, then shifts each row's/line's runs, underline rects and item subtrees by the leading offset plus the extra gap. `start`/`normal`/`stretch` pack at the start (no auto-track growth), `center`/`end`/`space-between`/`space-around`/`space-evenly` work in both grids and wrapping flex rows; `place()` already floors the box, so a wrapped flex container with `min-height` now places its lines inside that floor instead of leaving them at the top. A local page shows five 180px grids (start/center/end/between/evenly) and two wrapped flex columns (center/between). All five references keep their exact geometry and stay error-free: nextjs 7578px / 569 runs / 155.6 MB, MDN 2690px / 641 runs / 88.5 MB, Wikipedia 24575px / 10950 runs / 102.7 MB, HN 1695px / 728 runs / 78.2 MB, rust-lang 5800px / 483 runs / 85.3 MB; suite **804** default / **808** `engine-b + woff2` |
| 2026-11-05 | wave 57 (`justify-items` / `justify-self`) | a local five-grid alignment page + nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org | Grid items can now be aligned along the inline axis inside their grid area. `justify-items` parses into an `Align` on `BoxStyle` and `justify-self` into a per-item override (`auto` inherits, `normal` is `stretch`). `stretch` keeps the old full-area placement, so every existing page is byte-identical; `start`/`center`/`end` size the item to fit-content (declared px/% against the area, auto widths from max-content) and shift its margin box inside the area. Text items wrap at that fit-content width instead of the whole cell. A local page shows four grids (start/center/end/stretch) plus per-item overrides; suite **807** default / **811** `engine-b + woff2`. References: nextjs 7578px / 569 runs / 155.7 MB, MDN 2773px / 641 runs / 88.1 MB (the MDN homepage itself changed between waves: 2690px with the same binary when the feature is disabled, so this is content drift, not layout), Wikipedia 24575px / 10950 runs / 102.7 MB, HN 1695px / 728 runs / 78.2 MB, rust-lang 5800px / 483 runs / 85.2 MB |
| 2026-11-05 | wave 58 (`caption-side`) | a local three-caption table page + nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org | Table captions can now sit below the grid. `caption-side` parses `top`/`bottom` (plus `block-start`/`block-end`) into an optional value on `BoxStyle`; layout resolves a caption's own declaration first, then inherits the table's (the initial value stays `top`), and places bottom captions after the last row and its bottom border-spacing. Multiple captions keep document order within each side. A local page shows the default top caption, a table-level `caption-side: bottom`, and a caption-level `top` overriding a bottom table. All references stay error-free with unchanged geometry: nextjs 7578px / 569 runs / 155.5 MB, MDN 2773px / 641 runs / 88.2 MB, Wikipedia 24575px / 10950 runs / 103.4 MB, HN 1695px / 728 runs / 78.2 MB, rust-lang 5800px / 483 runs / 85.0 MB; suite **809** default / **813** `engine-b + woff2` |

| 2026-11-05 | wave 59 (live collections / named form access) | a local collections/form page + nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org | `getElementsByTagName`, `getElementsByClassName`, `getElementsByName`, `document.forms` and `form.elements` now return LIVE `HTMLCollection`-shaped objects: `length`, index reads, `item()`, `namedItem()` and named property reads re-query the DOM, so nodes appended after the collection was returned appear without re-querying. `document.forms.login` and `form.elements.email` work by name/id; `form.elements` is a getter backed by the form's `button/fieldset/input/object/output/select/textarea` descendants. `for..of`, spread and `Array.from` snapshot through one query; generic `Array.prototype.*.call(collection, ...)` (MDN's header-anchor code) still works. `querySelectorAll` stays a static NodeList. Fixing collection indices also exposed an old document-order bug in element-scoped subtree walks, now corrected. A local page shows live length growth, named form/control access, iteration and a generic array method. References stay error-free with unchanged geometry: nextjs 7578px / 569 runs / 155.1 MB, MDN 2773px / 641 runs / 87.7 MB, Wikipedia 24575px / 10950 runs / 102.9 MB, HN 1695px / 728 runs / 78.2 MB, rust-lang 5800px / 483 runs / 84.7 MB; suite **812** default / **816** `engine-b + woff2` |

| 2026-11-05 | wave 60 (`linear-gradient` backgrounds) | a local gradient page + nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org | Backgrounds can now use `linear-gradient(...)` in both `background` and `background-image`. The value parser accepts `to <side>`/corner directions and `deg`/`grad`/`rad`/`turn` angles, percentage stop positions, hard stops, named/hex/`rgb()`/`hsl()` colors and `rgba()`/`transparent` alpha; missing positions stay at 0%/100% or are distributed between neighbours. The gradient is stored as an `Arc<Gradient>` on `BoxStyle` and rides the existing rect channel through a parallel `rect_grad` slot, so stacking order, transforms, overflow clipping and sticky pinning are shared with solid backgrounds. Paint inverse-maps each pixel to the gradient line (through the box centre, length = the box's extent along the direction) and blends stops source-over, so `transparent` fades reveal the background colour; rounded corners reuse the rect culling rule. A local page shows three-stop, angled, corner, hard-stop, `to top`/`0.25turn`, alpha-fade and transformed cases. References stay error-free with unchanged geometry: nextjs 7578px / 569 runs / 155.3 MB, MDN 2773px / 641 runs / 87.5 MB, Wikipedia 24575px / 10950 runs / 103.2 MB, HN 1695px / 728 runs / 78.2 MB, rust-lang 5800px / 483 runs / 85.1 MB; suite **816** default / **820** `engine-b + woff2` |

| 2026-11-05 | wave 61 (`line-clamp`) | a local clamped-card page + nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org | A box can now cap how many text lines it shows. `line-clamp: N` (any single-dash vendor spelling unprefixes to it) parses into an optional positive count on `BoxStyle`; `none`, garbage and `0` clear it. After the box's flow is laid out, `place()` groups the subtree's runs by their line top; when more than N lines exist it clears every run below the cut, trims the kept line's last run with `fit_prefix` so a trailing marker fits inside the content edge, zeros underlines below the cut, and shrinks the content height to the cut so following siblings move up. `display: flex/grid` containers ignore the clamp. A local page shows a two-line card, a three-line card, an unclamped sibling and a short paragraph that already fits. References stay error-free with unchanged geometry: nextjs 7578px / 569 runs / 155.5 MB, MDN 2773px / 641 runs / 87.9 MB, Wikipedia 24575px / 10950 runs / 102.8 MB, HN 1695px / 728 runs / 78.3 MB, rust-lang 5800px / 483 runs / 84.8 MB; suite **818** default / **822** `engine-b + woff2` |

| 2026-11-05 | wave 62 (`radial-gradient`) | a local radial page + nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org | Backgrounds can now use `radial-gradient(...)` in `background` and `background-image` alongside the linear form. The parser accepts `circle`/`ellipse`, the `closest-side`/`farthest-side`/`closest-corner`/`farthest-corner` extents (plus the `contain`/`cover` synonyms), one or two explicit radii, and `at <position>` using the `background-position` grammar; a lone length implies a circle and a headless gradient defaults to a centred ellipse at farthest-corner. The extra geometry is one optional `Radial` field on the existing `Gradient`, so linear parsing, storage and the paint channel are unchanged. Paint resolves the centre and radii against the painted border box and maps each pixel through the normalized distance `sqrt(((x-cx)/rx)^2 + ((y-cy)/ry)^2)`, so `t` is 0 at the centre and 1 on the ending shape; rounded corners, stacking, transforms and sticky pinning reuse the solid-background path. A local page shows an off-centre circle, the default ellipse, explicit `20px 40px` radii, `closest-side`, positioned stops, a rounded-corner clip and a headless gradient. References stay error-free with unchanged geometry: nextjs 7578px / 569 runs / 155.7 MB, MDN 2773px / 641 runs / 89.8 MB, Wikipedia 24575px / 10950 runs / 103.0 MB, HN 1695px / 728 runs / 78.2 MB, rust-lang 5800px / 483 runs / 84.7 MB; suite **820** default / **824** `engine-b + woff2` |

| 2026-11-05 | wave 63 (JS allocation units) | nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org | The two hottest JS allocation units got smaller without changing behavior. `ObjKind`'s rare large payloads (`PromiseState` 56 B, `IterState` 40 B, `TypedArray` 32 B and an error's two fat name/message strings) now live behind one pointer each, so `ObjKind` falls from 56 to 32 bytes and `Obj` from 88 to 64 (−24 B for every object, including plain objects and arrays). Error name/message share one `Rc<ErrorData>`. `Param`'s optional destructuring pattern and default expression are boxed, so `Param` falls from 144 to 40 bytes (−104 B per parameter); the boxes allocate only when a function actually uses a default or pattern. A new size-guard test pins these ceilings. All 820 behavior tests still pass. References keep their exact geometry: nextjs 7578px / 569 runs / 153.2 MB, MDN 2773px / 641 runs / 87.3 MB, Wikipedia 24575px / 10950 runs / 102.5 MB, HN 1695px / 728 runs / 77.5 MB (67-83 MB across reloads), rust-lang 5800px / 483 runs / 84.9 MB; suite **821** default / **825** `engine-b + woff2` |

| 2026-11-05 | wave 64 (`Stmt`/`Closure` size) | nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org | The AST statement and closure units got smaller. `StmtKind`'s inline expressions are now boxed: `if`/`while`/`do` tests, `for` condition and update, `for-in`/`for-of` sources, `switch` discriminant, `throw`/`return` values and a class accessor owner; `try`'s catch clause is boxed too. That drops `StmtKind` from 176 to 56 bytes and `Stmt` from 192 to 72 (−120 B, −62%) for every statement, with an extra allocation only on statements that actually carry one of those expressions. `Closure.this` is now an optional boxed value: only arrow functions store a captured `this`, so `Closure` falls from 112 to 96 bytes. The size-guard test pins both ceilings. All 820 behavior tests still pass. References keep their exact geometry at the default viewport: nextjs 7578px / 569 runs / 148.2 MB (down from 153.2), MDN 2773px / 641 runs / 86.9 MB, Wikipedia 24575px / 10950 runs / 102.4 MB, HN 1695px / 728 runs / 78.3 MB (within the usual 67-83 MB reload range), rust-lang 5800px / 483 runs / 84.7 MB at 1000x616 (a later window resize to 1920x948 produced the shorter 4979px relayout, so that number is not comparable); suite **821** default / **825** `engine-b + woff2` |

| 2026-11-06 | wave 65 (partial `opacity`) | a local opacity page + nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org | Boxes can now paint at a partial `opacity` instead of the old all-or-nothing rule. The cascade stores a clamped 0..1 value on `BoxStyle` (still turning `0` into `display:none`), and one post-build pass composes it down the box tree into `BoxNode.eff`. Every primitive the box emits multiplies that effective alpha in: background/border/outline/caret colours ride the previously unused high byte of the existing `0x00RRGGBB` words, gradients and shadows scale their own alpha, images scale per-pixel alpha, and text runs carry an alpha byte that scales glyph coverage (decorations are alpha-packed too). Fully opaque primitives keep their old unpacked bytes, so paint output for pages without `opacity` is unchanged. A local page shows 50% red over white = 255,127,127, 25% black = 191,191,191, nested 50% over green = 128,127,0 and 50% text ink. References keep their exact geometry: nextjs 7578px / 569 runs / 148.8 MB, MDN 2773px / 641 runs / 89.2 MB, Wikipedia 24575px / 10950 runs / 103.0 MB, HN 1695px / 728 runs / 78.2 MB, rust-lang 5800px / 483 runs / 84.7 MB; suite **823** default / **827** `engine-b + woff2` |

| 2026-11-06 | wave 66 (observed CSS misses + border longhands) | a local border-longhand page + nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org | The engine can now report what it drops: `MAZDA_MISS=1` counts every property name rejected by the `KNOWN` gate and every at-rule the parser does not implement, then prints a frequency-sorted table after each load. The first reference run ranked 111 distinct dropped properties on nextjs.org, 38 on Wikipedia, 23 on rust-lang.org, 20 on MDN and 4 on Hacker News; transitions/animations, the individual `translate`/`rotate`/`scale` properties, `user-select`/`pointer-events`, the `font` shorthand and `filter` led the list. The cheapest visual win in it was borders: the model already stores per-side widths, but the per-side width/colour longhands and all four corner-radius longhands were missing from `KNOWN` (the handler had an unreachable top-left arm), so reset/Tailwind styles like `border-top-width: 1px` drew nothing. All eight per-side width/colour longhands and all four corner-radius longhands now parse into the existing per-side-width / one-colour / one-radius model (style words remain collapsed to solid). A local page verifies a 6px top border, a 10px left border, a per-side red colour and 12/20px corner clipping. Distinct dropped-property counts fall to 94 on nextjs.org and 10 on rust-lang.org; all five references keep identical geometry: nextjs 7578px / 569 runs / 148.8 MB, MDN 2773 / 641 / 86.9 MB, Wikipedia 24575 / 10950 / 102.6 MB, HN 1695 / 728 / 78.2 MB, rust-lang 5800 / 483 / 84.7 MB; suite **825** default / **829** `engine-b + woff2` |

| 2026-11-06 | wave 67 (individual `translate`/`scale` + rect edge clipping) | a local transform page + nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org | The top visual item in wave 66's miss ranking is implemented: the individual `translate` and `scale` properties now parse and paint, and `rotate` is accepted as a documented no-op (rotation still needs affine glyph/rect rasterization). Each property keeps its own `BoxStyle` field so the cascade keeps them independent of the `transform` list (`transform: none` does not clear a prior `translate`), and paint composes all three in spec order (translate outermost): the list's scale multiplies with `scale`, its offset is scaled by `scale`, and `translate` is added last. `%` offsets resolve against the box's own w/h, and single values follow CSS (`translate: 10px` leaves y at 0, `scale: 2` scales both axes; percentages divide by 100). Verifying the scale render exposed a painter bug: the solid-rect fill clamped a negative origin to pixel 0 but kept the full width/height, so any box a transform pushed past the left or top edge smeared its full size from the edge. The rect fill is now one helper that intersects the span with the buffer (gradients already did), and underline/caret rules share it. A local page verifies a 20/10px translate, a 2x scale about the center with the correct intersection at the page edge, and `rotate: 45deg` painting unrotated; `MAZDA_MISS` reports zero dropped names for it. Dropped-name counts fall to 91 on nextjs.org (was 94), 31 on Wikipedia (38) and 17 on MDN (20); references keep identical geometry: nextjs 7578px / 569 runs / 149.4 MB, MDN 2773 / 641 / 86.9 MB, Wikipedia 24575 / 10950 / 102.8 MB, HN 1695 / 728 / 78.2 MB, rust-lang 5800 / 483 / 84.9 MB; suite **828** default / **832** `engine-b + woff2` |

| 2026-11-06 | wave 68 (the `font` shorthand + size keywords) | a local font-shorthand page + nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org | The largest remaining visual miss (80 declarations on MDN) is implemented. `font` parses `[style || variant || weight || stretch]? size [/ line-height]? family`, sets size, line-height, style, weight and family together, and resets the components it omits; a value that doesn't parse (including a missing family) and the system keywords are dropped whole, so earlier longhands survive. The family tail goes through the existing comma-stack interner, so `font: 16px Arial, sans-serif` and `font-family: Arial, sans-serif` share one id. Size detection distinguishes a bare weight (`700`) from a size (only a unit, `%`, a size keyword or `0` is a size), and all three slash spellings (`16px/1.5`, `16px /1.5`, `16px / 1.5`) work. `font-size` gains the absolute-size keywords (`xx-small`…`xxx-large`, `larger`/`smaller`), and `line-height` moved to one helper that now accepts unitless, px/em/rem/%, viewport units and `normal`. A local page verifies 24px/1.5 → a 36px box, `700 1.2rem` → 19.2px with the line-height reset, `2vw/2` → 20px/40px, and the invalid `700 16px` (no family) skipped. Dropped-name counts fall to 90 on nextjs.org, 16 on MDN, 30 on Wikipedia and 9 on rust-lang.org. Wikipedia's total height changes from 24575 to 24673 because its `line-height: 1.4em` declarations now resolve (previously they fell back to the 1.35 default); the other references keep their heights: nextjs 7578px / 569 runs / 149.2 MB, MDN 2773 / 641 / 87.1 MB, Wikipedia 24673 / 10951 / 103.5 MB, HN 1695 / 720 / 78.4 MB, rust-lang 5800 / 483 / 85.0 MB; suite **832** default / **836** `engine-b + woff2` |

| 2026-11-06 | wave 69 (`filter` colour functions) | a local filter page + nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org | The other top visual miss is implemented. `filter` parses its function list into a single affine RGB map plus one alpha multiplier, so `grayscale`, `sepia`, `saturate`, `hue-rotate`, `invert`, `opacity`, `brightness` and `contrast` compose exactly and in order without a per-pixel function dispatch. `blur`, `drop-shadow` and `url()` are parsed and dropped (no offscreen buffer yet). One post-build pass composes each box's filter with its ancestors' into the same effective state that already carries `opacity`; the filter's alpha goes into that effective alpha once, while the stored RGB map is normalised to alpha 1 so children do not double-apply it. Every primitive the box emits is transformed: solid background/border/outline/shadow colours at layout emission, gradient stops as filtered clones (the affine map commutes with interpolation), images per pixel in the render blit, and text/underline colours by mutating the leaf's style before build. A filtered plain inline is promoted to a real box so icon-span filters paint. `filter: opacity(0)` early-outs all of that box's primitives. Dropped-name counts fall to 89 on nextjs.org, 15 on MDN, 29 on Wikipedia, 3 on Hacker News and stay at 9 on rust-lang.org (no `filter` there); zero `filter` declarations remain in `MAZDA_MISS`. References keep identical geometry: nextjs 7578px / 569 runs / 147-148 MB, MDN 2773 / 641 / 77-88 MB, Wikipedia 24673 / 10951 / 101-103 MB, HN 1695 / 720 / 67-78 MB, rust-lang 5800 / 483 / 77-85 MB (RAM is noisy between runs); suite **838** default / **842** `engine-b + woff2` |

| 2026-11-06 | wave 70 (`place-items`/`place-self` + grid `align-self`) | a local place-alignment page + nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org | The two alignment shorthands are implemented. `place-items: <align-items> <justify-items>?` sets the container's block- and inline-axis alignment, and `place-self: <align-self> <justify-self>?` sets the per-item overrides; with one value both axes take it, with two the first is block and the second inline. `place-self` treats `auto` as meaningful (it clears an earlier override) and drops the declaration when either value is unknown, so a shorthand never half-applies. Checking the result exposed a real grid bug: the row cross-axis pass read the container's `align-items` only, so a grid item's `align-self` did nothing (the flex path already honoured it). Grid now resolves `align-self` per item before shifting, which is what MDN's `place-self: center start` / `center end` grid arrows rely on. Dropped-name counts fall to 88 on nextjs.org (was 89) and 13 on MDN (15); Wikipedia 29, Hacker News 3 and rust-lang.org 9 are unchanged. References keep identical geometry: nextjs 7578px / 569 runs / 147 MB, MDN 2773 / 641 / 85 MB, Wikipedia 24673 / 10951 / 100 MB, HN 1695 / 720 / 76 MB, rust-lang 5800 / 483 / 82 MB; suite **840** default / **844** `engine-b + woff2` |

| 2026-11-06 | wave 71 (`background-clip`) | a local background-clip page + nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org | `background-clip` now clips the background: `border-box` (initial, unchanged), `padding-box` stops the colour/gradient at the padding edge and `content-box` stops it at the content edge. The colour and gradient layers are single rects, so the wave computes one `bg_box` and emits it instead of the border box; the background IMAGE keeps its padding-box sizing (`%` sizes/positions resolve where CSS says) and is cropped by passing the content window down to the placeholder child when the clip is `content-box`. A rounded box with a border still paints the ring: the border colour fills the border box and the colour layer paints inside it, with `content-box` moving that inner rect to the content box and shrinking the radius by border + padding (max-inset approximation). `background-clip: text` is deliberately NOT accepted - it needs a glyph-shaped mask this renderer has no place to build, and accepting it would paint a rectangle behind invisible text; it stays a dropped declaration. The `background` shorthand resets `background-clip` with the rest of the background longhands. A local page with a 6px transparent border and 10px padding verifies the three clip boxes: red fills 100px (border box), blue paints 88px at x=6 (padding box), green paints 68px at x=16 (content box) and the gradient follows the same clip; the capture matches. Dropped-name counts: 87 on nextjs.org (was 88), 13 on MDN, 29 on Wikipedia, 3 on Hacker News, 9 on rust-lang.org. References keep identical geometry: nextjs 7578px / 569 runs / 147 MB, MDN 2773 / 641 / 84 MB, Wikipedia 24673 / 10951 / 100 MB, HN 1695 / 720 / 76 MB, rust-lang 5800 / 483 / 82 MB; suite **842** default / **846** `engine-b + woff2` |

| 2026-11-06 | wave 72 (`text-shadow`) | a local text-shadow page + nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org | `text-shadow` now paints: up to TWO layers per run (`none`/garbage clears, extra layers dropped), inherited like the rest of the text properties and resolved at paint time — a layer with no colour is `currentcolor`. The style field is a fixed `[Option<TextShadow>; 2]` so `Style` stays `Copy` (a `Vec` would have forced `.clone()` through the whole box/run pipeline) and costs ~40 bytes per style with no heap. Paint stamps the run's glyph coverage into one small plane, box-blurs it per layer (three passes ≈ a Gaussian with σ = blur/2, the CSS definition of the blur radius) and composites at the layer's offset UNDER the run's own glyphs; the first layer composites last so it stays topmost, exactly as CSS orders them. The glyph walk moved into a shared `for_each_glyph` helper so the letter/word-spacing arithmetic — which must stay bit-identical to the width layout measured — is written once instead of twice. A local page verifies a hard offset shadow (the lit pixels are exactly the ink moved by dx/dy, with the glyph ink still painted over it), a blurred glow, two stacked layers and a `currentcolor` glow; the capture matches. Dropped-name counts: 86 on nextjs.org (was 87, `text-shadow` gone), 13 on MDN, 29 on Wikipedia, 3 on Hacker News, 9 on rust-lang.org. References keep identical geometry: nextjs 7578px / 569 runs / 147 MB, MDN 2773 / 641 / 84 MB, Wikipedia 24673 / 10951 / 101 MB, HN 1695 / 720 / 76 MB, rust-lang 5800 / 483 / 83 MB; suite **844** default / **848** `engine-b + woff2` |

| 2026-11-06 | wave 73 (`grid-auto-flow`) | a local grid-flow page + nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org | Auto-placed grid items now pack along either axis. `grid-auto-flow: [row|column] || dense` parses (tokens in any order; `dense` is accepted but NOT stored — the sparse cursor here never passes a free cell, since auto items are 1×1 and there are no row spans to leave a hole behind, so dense packing would place items identically; documented on the `GridFlow` enum), an unknown/repeated token drops the whole declaration so an earlier value survives, and `dense` alone means `row dense`. `grid()` keeps the base column count as `nc0` for the placement cursors, and column flow advances the auto cursor row-first, wrapping to the next column at `nr_flow` = explicit rows, else area rows, else one implicit row. Because column flow can place items past the explicit template, the column count is re-derived from the placed spots (`nc = nc0.max(max c1)`) and `tracks` grows `Track::Auto` to match — a no-op in row flow (the cursor never exceeds `nc0`), so every reference row stays byte-identical. A local page verifies row flow over a 2×2 template (a b / c d), column fill (x y z side by side on one row), column wrap at two explicit rows (a c / b d), `dense column` in either token order, and an invalid second value keeping `column`. Dropped-name counts fall to 85 on nextjs.org and 12 on MDN (both had `grid-auto-flow`; Wikipedia 29, Hacker News 3, rust-lang.org 9 unchanged, zero `grid-auto-flow` misses anywhere). References keep identical geometry: nextjs 7578px / 569 runs / 147 MB, MDN 2773 / 641 / 85 MB, Wikipedia 24673 / 10951 / 102 MB, HN 1695 / 720 / 76 MB, rust-lang 5800 / 483 / 83 MB; suite **848** default / **852** `engine-b + woff2` |

| 2026-11-06 | wave 74 (`backdrop-filter`) | a local frosted-glass page + nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org | `backdrop-filter` blurs what is already painted behind a box. The value parser (`parse_backdrop`) keeps only the largest `blur()` radius (`blur()` with no argument = 0), accepts the other filter functions without effect (`drop-shadow`/`url`/`grayscale`/… parse but contribute nothing — the mirror image of `filter: blur()`), and an unknown function or malformed token drops the whole declaration so an earlier value survives; `none`/`initial`/`unset` clears it. `BoxStyle.backdrop` holds the radius (0 = none), and `KNOWN` gains the name. Paint is a phase split driven by new `BackdropGroup` records — DFS-contiguous spans over the five paint channels plus the clip-intersected border-box region, recorded exactly like `StickyGroup` and remapped through `zsort` (a split-z subtree disables the group instead of blurring the wrong pixels). Each `want_fixed` pass now: phase 1 paints everything except the deferred spans, `blur_backdrops` snapshots each region (clamp-to-edge), runs three box passes of radius ≈ blur over the three colour planes and writes the blurred pixels back inside the rounded border box, then phase 2 paints the withheld subtree sharp on top. Pages without the property record no groups, so phase 1 is the plain paint and phase 2 never runs — output stays byte-identical. A local page verifies a sticky frosted nav (blur 6px) and a frosted rounded panel (blur 8px) laid over a hard repeating-gradient edge: the stripes blur to a soft gradient inside the panel, stay sharp outside it, and the panel's own text is sharp; the capture matches. Dropped-name counts: 84 on nextjs.org (was 85, `backdrop-filter` gone), 12 MDN, 29 Wikipedia, 3 Hacker News, 9 rust-lang.org — zero new misses, no `backdrop-filter` miss anywhere. References keep identical geometry: nextjs 7578px / 569 runs / 146 MB, MDN 2773 / 641 / 84 MB, Wikipedia 24673 / 10951 / 101 MB, HN 1695 / 720 / 75 MB, rust-lang 5800 / 483 / 87 MB; suite **852** default / **856** `engine-b + woff2` |

| 2026-11-06 | wave 75 (JS observed-miss instrumentation) | nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org (no local page — nothing paints differently) | The JS half of the wave-66 P0 ships: `MAZDA_MISS=1` now also ranks names the JS engine could not resolve, in two buckets. `global` = a bare identifier that resolved nowhere (`env_get` falling through to `undefined`) plus reads on the `window` object itself; `member` = a read that came back `undefined` on an object the engine built — classified by `miss_bucket`, which counts the global object, any object with an own `__node` marker or a native method/accessor (element wrappers, the document/navigator/location/history/storage singletons built from natives alone, and native-backed built-ins such as `Object`/`Math`), and `Live`/`Style` kinds, while a plain script object's undefined read records nothing — that is ordinary JavaScript (feature detection on your own object). Two exclusions keep the census honest: the name `undefined` (it reads as itself by definition, bare or as `window.undefined`) and `#`-prefixed keys (engine internal slots such as `#new.target`). Each bucket keeps 512 distinct names with counts; past the cap already-seen names keep counting and a running `over_*` tally reports how many distinct names were dropped, so a page synthesizing `window['k'+i]` cannot grow the census without bound. `js::dump_misses()` runs beside `css::dump_misses()` after every navigation and prints each bucket ranked by frequency then name (`[js-miss] {n} distinct missing {bucket} names` … `[js-miss] {bucket} {count} {name}`). Engine A only — the hooks live in `env_get`/`get_member_impl`, engine B resolves its own lookups — and off by default: recording is one cached-bool branch at each hook, nothing allocated. The two new tests drive pure local-state helpers (`miss_bucket`, `record_js_miss`, `report_js_bucket`) so parallel tests never race the process-wide census. Suite **854** default / **858** `engine-b + woff2`. Reference runs with `MAZDA_MISS=1` keep byte-identical geometry and CSS counts: nextjs 7578px / 569 runs / 146.4 MB, MDN 2773 / 641 / 84.7 MB, Wikipedia 24673 / 10951 / 101.0 MB, HN 1695 / 720 / 76.0 MB, rust-lang 5800 / 483 / 82.5 MB; `[css-miss]` 84 / 12 / 29 / 3 / 9 unchanged. First JS census: nextjs 61 distinct missing globals (top: `process` 6, `Reflect` 5, `__REACT_DEVTOOLS_GLOBAL_HOOK__` 3, `reportError` 3) and 58 members (top: a page-built `Symbol(…)_<n>` key 8, `@@iterator` 4, `deleteAll` 4, `_reactRetry`/`find`/`getOwnPropertySymbols`/`isExtensible` 3); MDN 1 global (`trustedTypes`) / 0 members, Wikipedia 2 (`requestIdleCallback`, `RLQ`) / 1 (`timing`), HN 0 / 1 (`innerHTML`), rust-lang 1 (`exports`) / 0 |

| 2026-11-06 | wave 76 (`Reflect`) | nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org (no local page — nothing paints differently) | The top actionable global from the wave-75 census ships: `Reflect` with all thirteen standard methods (`apply`, `construct`, `defineProperty`, `deleteProperty`, `get`, `getOwnPropertyDescriptor`, `getPrototypeOf`, `has`, `isExtensible`, `ownKeys`, `preventExtensions`, `set`, `setPrototypeOf`). It is routing, not a second implementation: `get`/`set`/`deleteProperty` call `get_member`/`set_member`/`delete_member`, `defineProperty` shares `define_prop`, `ownKeys`/`getOwnPropertyDescriptor`/`getPrototypeOf`/`setPrototypeOf` delegate to the same `n_object` arms the `Object` statics use, and `construct` was extracted from the `new` operator so both build the fresh `this`, set `new.target` (`pending_construct`) and apply the "object result wins" rule — `Reflect.construct` ignores its optional `newTarget` (documented shortcut). `apply`/`construct` take a real array as the argument list (the engine has no array-likes and no `arguments` object) and throw TypeError otherwise. The `in` operator's per-type walk moved into a `has_member` helper that `Reflect.has` shares, so the operator and the meta-API cannot drift. `isExtensible`/`preventExtensions` are constants — there are no integrity levels (`freeze`/`seal` are identity too). Suite **856** default / **860** `engine-b + woff2`; two new tests cover apply/construct/error paths and the object operations against the operator semantics. References keep identical geometry and CSS counts: nextjs 7578px / 569 runs / 146.7 MB, MDN 2773 / 641 / 75-93 MB (two runs — the page's script timing decides how much executes), Wikipedia 24673 / 10951 / 101.2 MB, HN 1695 / 720 / 76.0 MB, rust-lang 5800 / 483 / 82.5 MB; `[css-miss]` 84 / 12 / 29 / 3 / 9 unchanged. The census confirms the fix: nextjs globals 61 → 60 with `Reflect` the only name removed, and exposes the next layer — nine `Reflect` decorator-metadata names (`defineMetadata`, `deleteMetadata`, `getMetadata`, `getMetadataKeys`, `getOwnMetadata`, `getOwnMetadataKeys`, `hasMetadata`, `hasOwnMetadata`, `metadata`) now read as member misses because core-js's metadata module only probes them once `Reflect` exists (nextjs members 58 → 67; the other four sites unchanged) |

| 2026-11-06 | wave 77 (`noscript` raw text + `Array.prototype` extras + google joins the reference set) | nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org + google.com (new baseline) | google.com became the sixth reference page, and its first capture exposed a real parser bug rather than a CSS one: with scripting enabled, `<noscript>` content must stay TEXT, but the parser built it as elements, so google's fallback `<style>table,div,span,p{display:none}</style>` was collected as a stylesheet and hid the whole page (1 box, 0 runs). `noscript` joined `RAW_TEXT` (alongside `script`/`style`/`textarea`/`title`/`svg`), so its `<style>` and `<meta http-equiv=refresh>` are inert; google then lays out 835 nodes / 115 boxes / 1400px / 28 runs at 95.8 MB, with `[css-miss]` 50 names and `[js-miss]` 20 globals / 11 members. The wave also implements the nine `Array.prototype` methods pages probe (`find`, `findIndex`, `findLast`, `findLastIndex`, `at`, `fill`, `copyWithin`, `flat`, `flatMap`), carried by two helpers: `rel_index` (relative index grammar for `at`/`fill`/`copyWithin` - negative from the end, out-of-range clamps, NaN -> 0) and `flatten_into` (shared by `flat`/`flatMap`, depth clamped to 64 so a script-built nested array cannot recurse without bound; deeper nesting keeps the rest as arrays). `copyWithin` snapshots its source range before writing, `fill` reuses `slice_bounds` and returns the receiver, and the find family follows the engine's existing iterator convention (callback gets element/index/thisArg, `this` is the global object). The census confirms it: nextjs members 67 -> 61 with `at`/`copyWithin`/`fill`/`findIndex`/`flat`/`flatMap` gone and no new names. Suite **861** default / **865** `engine-b + woff2`; five new tests (four array, one parser). The first capture attempt happened during a host-wide TLS outage (every host and tool failed, `curl`/`Invoke-WebRequest`/`git ls-remote` included) and was retaken once egress returned; the five older references keep identical geometry (nextjs 7578/569, MDN 2773/641, Wikipedia 24673/10951, HN 1695/720, rust 5800/483) and CSS counts (84/12/29/3/9). google's new baseline still has large visual gaps: its search form lays out 968px tall and two large black SVG/image fills stretch over the page - a layout/SVG item for wave 78, not a parser one. |
| 2026-11-07 | wave 78 (SVG intrinsic sizing, flex `%` basis, transparent borders) | google.com + nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News + rust-lang.org | google.com's three visible defects were all sizing/paint rules, not site quirks. (1) `svg::probe` treated `viewBox` numbers as pixels, so `viewBox="0 -960 960 960"` icons became 960x960; a viewBox is user space and now contributes only its RATIO — width/height attributes win, a missing axis follows the ratio, and a viewBox-only root gets `width: 100%` of its containing block (`svg::fills_width`, applied in layout) with the height from the ratio. Ground truth came from headless Chrome on a local page (no window, no input): `<svg>` without viewBox 300x150; viewBox-only 600x600 in a 600px block and 24x12 for 2:1 in 24px; `width="80"` + 2:1 viewBox 80x40; `height="30"` 60x30; a 20px flex button holding `svg{width:100%}` 20x20. (2) A flex item's `width: %` basis was floored by its intrinsic width and capped against its siblings, so a 20px icon button's SVG became 300px and three `width:100%` columns (rust-lang's "Why Rust?" row) each collapsed to 0; the percentage is now the definite basis with no floor and no cap, and the existing `shrink_row` pass splits the overflow the way `flex-shrink: 1` does (each of the three takes a third). (3) `border: Npx solid transparent` painted currentColor, so google's `border-bottom: 8px solid transparent` drew a 444x8 black bar; `BoxStyle.btransparent` keeps the side's width (it still sizes the box) and drops only the paint. A fourth, related fix: a replaced element's intrinsic width is now clamped by `max-width`/`min-width` (rust-lang's 512px logos under `max-width:128px` laid out 512px wide and tall). google.com goes 1400px -> 488px (Chrome measures 525px for the same markup), form 56px, icons 36x36 / 24x24 / 20x20, and the capture shows a clean homepage. Suite **866** default / **870** `engine-b + woff2`; five new tests plus the extended border-paint snapshot. The other references keep their geometry except rust-lang (5800 -> 5344: the image ratios are now real), and CSS counts 84/12/29/3/9/50 are unchanged. |

| 2026-11-07 | wave 79 (real em scale + baseline, `rem` and unitless `line-height`, flex `%` limits and content-based shrink, hyphen breaks, declared `height`) | rust-lang.org + google.com + nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News | Text had been laid out and painted at the wrong size all along: ab_glyph's `PxScale` targets the font's HEIGHT (ascent - descent), not the CSS em square, so every run was rasterized at `em/height` (Fira 5/6 = 0.833, Alfa Slab 0.73) and its baseline came from that scaled height. `em_scale` now converts `size x height_unscaled / units_per_em`, and the baseline uses the real font ascent plus CSS half-leading; Chrome canvas `measureText` on the live webfonts is the ground truth (Fira `Install@18` 51.10px, `Governance@18` 96.25px, `empowering@36` 201.82px; Alfa `Rust@18` 44.21px, `@96` 235.78px) and the engine now matches it. Pages whose font sizes were already right grew into their real text instead of looking undersized (nextjs 7578 -> 7906, MDN 2773 -> 2966, Wikipedia 24673 -> 28074). `rem` now resolves against the `<html>` element's computed font-size (the cascade visits `<html>` first and stores it in a CSS thread-local), so rust-lang.org's `html{font-size:62.5%}` + `html{font-size:75%}` = 12px and `body{font-size:1.5rem}` = 18px match Chrome (previously `rem` used the 16px default and the body was 24px); a unitless `line-height` is stored as a factor and resolved against the element's FINAL font-size (`1.6 x 18 = 28.8px`), and the engine's own default font-size moves 15px -> 16px (the UA initial). Flex: a `%` `max-`/`min-width` on an item is pinned against the ROW before anything measures it (it used to be re-resolved against the item's own slot, capping rust's `max-width:50%` columns twice), the automatic minimum size is clamped by `max-`/`min-width` (rust's 512px logos under `max-width:96px` no longer force a 512px shrink floor), and shrink is weighted by the CONTENT base instead of the margin box, so three equal `width:100%` columns stay equal when the middle one carries 48px side margins (285.3px each vs Chrome 288.3px). UAX #14 hyphen break opportunities now split a compound word at the last hyphen that fits, in line layout and in the flex floor's min-content, so `performance-critical` stops forcing an 80px column. A declared px `height` finally sizes an auto block (`.highlight{height:12px}` bars were 0px tall; `%` heights stay ignored). rust-lang.org drops 5344 -> 4113px (Chrome 3841): nav 227 (258), green 527 (510), purple 744 (738), production 416 (379), get-involved 624 (609), thanks 550 (529), purple card widths 187/229/254/287 (190/230/254/287). Suite **875** default / **878** `engine-b + woff2`; eight new tests plus the extended grid `align-content` expectation.

| 2026-11-07 | wave 80 (UA margins + sibling margin collapsing, replacing the 0.6em paragraph gap) | rust-lang.org + google.com + nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News | The engine no longer invents spacing: the three synthetic `0.6 x font-size` paragraph gaps are gone, and real CSS margins do the job. Every `margin-*` longhand and shorthand now records which sides a declaration claimed (`decl::MARGIN_TOP/RIGHT/BOTTOM/LEFT`), and `ua_margins` runs at the end of the cascade — after the FINAL font-size is known, because the UA values are em-based — filling only the sides no declaration set. That is what a real UA rule loses to: a reset (`* { margin: 0 }`, or rust-lang.org's `p { margin: 12px 0 30px }`) wins wholesale, while a page that declares nothing gets the browser defaults (`p`/`dl`/`pre`/`address`/`ul`/`ol` 1em block, `blockquote`/`figure` 1em block + 40px inline, `h1` 0.67em, `h2` 0.83em, `h3` 1em, `h4` 1.33em, `h5` 1.67em, `h6` 2.33em, `dd` 40px start, `body` 8px). Ground truth came from headless Chrome's computed margins: rust body 0 / p 12/30 / h2 24/0, Hacker News body 8px on all sides, google body 0 / hr 20/7, nextjs body 0 / p 0/8, MDN body 0 / p 0 / h2 19.92/19.92 / h3 0/16 / figure 16/16 + 40px sides. In `block()` a `prev_mb` carries the previous in-flow sibling's bottom margin without adding it to `y`; the next sibling's gap is `collapse_margins(prev_mb, margin-top)` (two positives take the larger, two negatives the more negative, mixed ones add) and any line-level content resets `prev_mb` to 0. This removes the double counting that dominated real pages: every MediaWiki paragraph (`p { margin: 0.5em 0 }`) had a 25.6px boundary (8 + 8 + 9.6) where Chrome uses 8, so Wikipedia goes 28074 -> 21872px; nextjs 7906 -> 7810 (7258 on the first pre-JS layout), rust-lang.org 4113 -> 3572 (Chrome 3841 — the old synthetic gap overshot by 272, the correction now undershoots by 269, and that remaining section-spacing gap is the next layout target), MDN 2966 -> 2925 (3328), Hacker News 1782 -> 1772 (1217) — now with the UA 8px body inset, matching Chrome, and google.com 539 -> 514. Parent/first/last-child margins deliberately do not collapse through the parent (documented shortcut: the last child's held-back bottom margin is added inside the box, which is also what a BFC requires), and `hr` keeps its hard-break-leaf model with no UA margin. Suite **876** default / **879** `engine-b + woff2` (one new collapse test, ten expected-geometry updates) |

| 2026-11-07 | wave 81 (logical CSS properties, `text-wrap`, `appearance`, axis background positions, accepted no-op census) | rust-lang.org + google.com + nextjs.org + MDN homepage + Wikipedia `/wiki/JavaScript` + Hacker News | The largest remaining layout-miss cluster was logical CSS: `inline-size`/`block-size`, `inset-inline-start`, `margin-before`, `border-inline-start-width` and the two-sided shorthands were dropped by the `KNOWN` gate even though the engine already had every physical property they alias. A new `logical_physical` table renames the one-sided names BEFORE the gate (writing-mode is assumed `horizontal-tb`, direction ltr: inline = left/right, block = top/bottom), including the legacy `before`/`after`/`start`/`end` spellings that browser-prefixed declarations unprefix into — that is exactly google.com's `margin-before` cluster. `inset-inline`/`inset-block`, `border-inline`/`border-block` and their `-width`/`-style`/`-color` longhands cannot be a 1:1 rename (they touch two sides), so `apply_decl` expands them: 1 value = both sides, 2 = start then end. `text-wrap: nowrap` now sets the inherited no-wrap mode (only `nowrap` is modeled; `wrap`/`balance`/`pretty` are hints), `appearance: none` suppresses the native-control chrome entirely (`BoxStyle::appearance_none`, checked at the top of `control_chrome`), and `background-position-x`/`-y` replace one axis while keeping the other. The miss census also gained a curated accepted-no-op list for properties with no static-layout effect in this engine (pointer-events, user-select, will-change, contain, backface-visibility, scroll-margin/padding, overscroll/touch/scrollbar/scroll-snap, legacy box-orient/pack/align, font smoothing, text-size-adjust, …), so the census now ranks real layout work. Distinct dropped properties: google 50 -> 16, nextjs 84 -> 43, MDN 12 -> 4; rust/HN/Wikipedia were already free of logical names and stay at 9/3/29. Geometry barely moves because the affected declarations sit on icons and decorative boxes: MDN 2925 -> 2926, google 514, nextjs 7810 (7258 pre-JS), rust 3572, Hacker News 1772, Wikipedia 21872 — unchanged. Suite **879** default / **882** `engine-b + woff2` (three new tests) |

### Wave 81 findings

- **Logical properties are the cheapest real CSS win available.** Every
  physical property they alias already existed, so one parse-time rename
  turns a whole family of dropped declarations into working layout without
  touching the cascade or layout code. The one-sided names map before the
  `KNOWN` gate; only the two-sided shorthands need real expansion in
  `apply_decl`.
- **`before`/`after`/`start`/`end` are the legacy alias spellings.** The
  prefix stripper turns `-*-margin-before` into `margin-before`, which is
  why google.com's census showed names no current spec uses. Mapping them to
  `block-start`/`inline-end` recovers those declarations.
- **`text-wrap` is line breaking, not whitespace handling.** `nowrap` shares
  the inherited `Ws` mode with `white-space: nowrap`; a later `wrap`/`balance`
  cannot restore wrapping without a second flag, documented as a shortcut.
- **`appearance: none` must suppress every chrome property.** Filling only
  the border would leave the UA fill, padding, radius and min-height behind;
  the early return in `control_chrome` is what makes an author-reset control
  look like a plain box.
- **The remaining census is honest.** After this wave the top names are
  animation/transition (no static-layout effect), `@container`/`@property`
  at-rules, counters, bidi, multi-column, grid row placement and font
  shaping — the real layout queue rather than aliases of implemented
  properties.

### Wave 80 findings

- **A synthetic paragraph gap double-counts every author margin.** The old
  block flow summed both siblings' margins and then added `0.6em`, so
  MediaWiki's `p { margin: 0.5em 0 }` produced 25.6px between paragraphs where
  Chrome produces 8. Removing the gap and collapsing real margins is what
  takes Wikipedia 28074 -> 21872px and nextjs 7906 -> 7810px. The gap was
  never measuring the truth; it was masking missing margin rules.
- **UA margins belong to the cascade, not to a tag table at layout time.** The
  values are em-based, so they need the element's FINAL font-size (an author
  `font-size` may beat the UA one), and they must fill only the sides an
  author declaration did not set. `decl::MARGIN_*` bits make that exact:
  rust's `p { margin: 12px 0 30px }` suppresses all four UA sides while
  Hacker News' unstyled `p` (it has none) would receive the defaults.
- **Sibling collapsing is the max / min / sum rule.** Two positive margins
  take the larger, two negatives the more negative, mixed signs add (CSS
  §8.3.1). `prev_mb` is deliberately not added to `y` when the child is
  placed — the next sibling owns the collapse — and any text, inline box or
  float resets it, because line boxes have no margins.
- **The body's own margin does apply at the root.** `place(root, 0, 0, …)` adds
  the root's resolved margins, so Hacker News' UA `body { margin: 8px }`
  insets the page exactly as Chrome does; before this wave the root box's
  margin was effectively invisible and HN was flush to the viewport.
- **Parent/child collapsing is a documented approximation.** A first child's
  top margin and a last child's bottom margin stay inside their parent instead
  of collapsing through a padding/border-free ancestor. With no parent margin
  the downstream flow position is the same; the visible differences are a
  parent background covering the margin and a parent margin adding to it
  instead of taking the max. Keeping the last child's bottom margin inside the
  box is also what a BFC/flex item/`overflow` box must do.
- **`hr` still has no box.** It remains a hard-break leaf with no painted rule,
  so it takes no UA margins (google's computed `hr` is 20px/7px and only adds
  blank space in the engine); the `<hr>` paint rule stays in the CSS queue.
- **The remaining rust-lang.org distance is section spacing, not
  paragraphs.** At 3572px the page is 269px short of Chrome's 3841px, while
  the individual sections measured in wave 79 are within ~5%. The 0.6em gap
  removal overshot in the other direction (4113 -> 3572), so the next spacing
  work is the section/heading rules that the gap used to approximate, not the
  paragraph margins this wave fixed.

- **`PxScale` is a font height, not the em square.** ab_glyph 0.2.32 scales by
  `height_unscaled()` (ascent - descent), so passing the CSS size painted Fira
  at 5/6 and Alfa Slab at 0.73 of its true size, and the baseline inherited the
  same error. Every run was therefore measured and rasterized small. The fix is
  one conversion (`size × height_unscaled / units_per_em`) plus a baseline from
  the real ascent and CSS half-leading. Ground truth was headless Chrome's
  `measureText` on the live webfonts after `document.fonts.load(...)`:
  `Install@18` 51.10px, `Governance@18` 96.25px, `empowering@36` 201.82px,
  Alfa `Rust@18` 44.21px, `@96` 235.78px. This is a correctness fix that makes
  already-correct pages taller: nextjs 7578 -> 7906, MDN 2773 -> 2966,
  Wikipedia 24673 -> 28074.
- **`rem` belongs to `<html>`, not to the UA default.** rust-lang.org sets
  `html { font-size: 62.5% }` and later `75%`, so the root is 12px and
  `body { font-size: 1.5rem }` is 18px — Chrome confirms 12/18/18 for
  html/body/nav. With `rem` pinned to 16px the body was 24px and every line
  was too long and too tall. The cascade now resolves `<html>` first and
  publishes its computed size to a CSS thread-local that `parse_size` reads;
  `1em` on a synthetic div still reports 18px, so the cascade is consistent.
- **A unitless `line-height` is a factor of the FINAL font-size.** It was
  stored as a negative factor and resolved against a stale size, so
  `line-height: 1.6` at 18px did not give 28.8px. `Style::line_height()` now
  multiplies by the element's resolved size, matching Chrome's 28.8px on
  body/nav/cards and 45px for the 36px h2.
- **A flex item's `%` limit resolves against the row, exactly once.**
  `max-width:50%` was left symbolic for `place()`, which resolved it against
  the item's own slot: two 50% columns each got ~472px slots and painted
  ~236px, doubling the section height. The flex pass now pins `%`
  `max-`/`min-width` to the row width as px before measuring.
- **Shrink is weighted by content, not by the margin box.** Three equal
  `width:100%` items, the middle one with 48px side margins, gave the middle
  item a bigger shrink weight and left the other two wider. Using the content
  base (outer width minus margins) makes all three 285.3px, against Chrome's
  288.3px; the purple cards come out 187/229/254/287 against 190/230/254/287.
- **`max-width` clamps the automatic minimum size.** rust's purple cards hold
  512px logos under `max-width:96px`; the uncapped file width became each
  card's shrink floor, so four `width:100%` cards could never fit their row.
  `content_min_w` and the declared-px branch of `min_content_w` now clamp by
  `max-`/`min-width` before the floor is taken.
- **A hyphen is a break opportunity (UAX #14).** `performance-critical` was
  one unbreakable token, so its 80px width was a column's min-content. The
  line breaker now takes the last hyphen that fits (falling back to a normal
  wrap when none does), min-content is the longest hyphen-free piece, and a
  break inside a word does not consume a line when the remainder still fits.
  Automatic (`hyphens: auto`) dictionary hyphenation remains unimplemented.
- **A declared px `height` sizes an auto block; `%` does not.** Section
  headers use `.highlight { height: 12px }`; the engine ignored plain heights,
  so the bars were 0px and every header came out short. A px-resolved height
  now sets the used content height (overflowing content still lays out), while
  a `%` height stays ignored because the engine does not track the containing
  block's height.
- **Measured.** Suite **875** default / **878** `engine-b + woff2`. rust-lang
  4113px / 485 runs / 78-83 MB (Chrome 3841), google 539 / 28 / 91-98 (Chrome
  fits it in the 616px viewport), nextjs 7906 / 571 / 151.4 (10548), MDN 2966
  / 643 / 89.8 (3328), Hacker News 1782 / 720 / 76-81 (1214), Wikipedia 28074 /
  10956 / 105.7 (Chrome unavailable: a Wikimedia error page). rust's remaining
  272px is mostly spacing: the legacy 0.6em paragraph gap still stands in for
  real UA margins and margin collapsing, which is the next headline fix.

### Wave 78 findings

- **A `viewBox` is a coordinate system, not a pixel size.** google's icon set
  uses `viewBox="0 -960 960 960"`; `probe()` read 960 as the width, so every icon
  laid out 960x960 (and one stretched the search field to 968px). The viewBox now
  contributes only its ratio: `width`/`height` attributes (or CSS) supply the
  axes, a missing axis follows the ratio, and a root with a viewBox but neither
  attribute defaults to `width: 100%` of its containing block — an inline `<svg>`
  fills a 20px flex button instead of taking the 300x150 object size. Ground
  truth (headless Chrome, local page, no window or input): no viewBox -> 300x150;
  viewBox-only -> 600x600 in a 600px block, 24x12 for 2:1 in 24px; `width="80"`
  + 2:1 -> 80x40; `height="30"` -> 60x30; flex 20px + `svg{width:100%}` -> 20x20.
- **The flex `%` basis needed no floor and no sibling cap.** `width:%` is a
  definite basis; the intrinsic width floored it (the 300px SVG again), and the
  "keep siblings on the row" cap from wave 77 zeroed every item when several
  siblings declared `width:100%` (rust-lang's three "Why Rust?" columns). The
  basis is now exactly the percentage and `shrink_row` splits the overflow by
  `flex-shrink` — three equal columns take a third each, and google's 20px icon
  button holds a 20x20 SVG.
- **A transparent border keeps its width and paints nothing.**
  `border: 8px solid transparent` has no colour token, so the shorthand fell back
  to currentColor and google's search field drew a 444x8 black bar.
  `btransparent` is a separate flag: the width still sizes the box, only the
  paint is skipped (a later real colour clears it).
- **`max-width` clamps a replaced element's intrinsic width too.** With the
  viewBox ratio honoured, rust-lang's 512px SVG logos (under `max-width:128px`)
  laid out 512px square; the intrinsic-width branch now passes through
  `max-width`/`min-width`. That page drops 5800 -> 5344 (Chrome 3841) and the
  logos match at 128x128.
- **Measured.** Suite **866** default / **870** `engine-b + woff2` (five new
  tests plus the extended border-paint snapshot). nextjs 7578px / 569 runs /
  146.5 MB, MDN 2773 / 641 / 85.1, Wikipedia 24673 / 10951 / 101.3, HN 1695 /
  720 / 76.7, rust-lang 5344 / 483 / 70.7, google 488 / 28 / 92.3; `[css-miss]`
  84/12/29/3/9/50 and google's `[js-miss]` 20 globals + 11 members are unchanged.
  Chrome's own heights for the same markup: google 525px, rust-lang 3841px —
  google is now close, rust still tall because whole sections stack instead of
  sitting side by side (flex/grid gaps, not SVG).

### Wave 77 findings

- **The first google capture was blocked, then the page was blank for a
  reason worth fixing.** The first attempt failed before any page existed:
  `[nav] fetch failed ... tls connection init failed: unexpected end of file`,
  and the engine painted its own "Failed to load" text (29px, 14 runs). That
  failure was not Google-specific: at the same moment `curl.exe` failed
  identically for `google.com`, `rust-lang.org`, `example.com`, `baidu.com`,
  `1.1.1.1` and `github.com` (`schannel: failed to receive handshake`),
  `Invoke-WebRequest` failed on send, and `git ls-remote` could not reach the
  repo - ICMP echo and TCP connects still worked, so the host's upstream was
  dropping HTTP/TLS payload. A 7-minute probe (20 attempts) recorded the
  outage, and the capture was retaken once egress returned. When it did, google
  loaded 835 nodes and then laid out **one box**: its `<noscript>` fallback
  contains `<style>table,div,span,p{display:none}</style>`, and the parser had
  been building noscript content as elements, so that style hid the page.
- **`<noscript>` is the fix, and it is a parser rule, not a site hack.**
  With scripting enabled the HTML parser keeps noscript content as text (only
  a scripting-disabled parser builds its elements). This engine always runs
  scripts, so `noscript` now sits in `RAW_TEXT` next to
  `script`/`style`/`textarea`/`title`/`svg`: the `<style>` is never collected,
  the `<meta http-equiv=refresh>` inside it never fires, and a page's non-JS
  fallback stays invisible text. Google then lays out 835 nodes / 115 boxes /
  1400px / 28 runs at 95.8 MB. A parser test pins the rule (one text child, no
  `style` element, one real `p`).
- **Google's baseline is honest about what still looks wrong.** The capture
  shows the logo, top links, account pill and search field, but the search
  form lays out 968px tall and two large black shapes (SVG/image fills, not yet
  diagnosed) stretch over the page: a layout/SVG item for wave 78 (the box dump
  shows `form h=968` inside an `h=1008` flex section). That is measured, not
  guessed - the `[box]` dump and the screenshot are the evidence.
- **The other half of the wave is the Array extras.** With google expected to
  be a fresh baseline, the wave also took the next self-contained cluster from
  the wave-75 census: nine `Array.prototype` methods pages probe (`find`,
  `findIndex`, `findLast`, `findLastIndex`, `at`, `fill`, `copyWithin`,
  `flat`, `flatMap`). Two helpers keep it small: `rel_index` resolves the
  relative index grammar once for `at`/`fill`/`copyWithin` (negative counts
  from the end, out-of-range clamps, NaN -> 0), and `flatten_into` serves both
  `flat` and `flatMap` with the depth clamped to 64 so a script-built nested
  array cannot recurse without bound (documented shortcut: deeper nesting
  keeps the rest as arrays). `copyWithin` snapshots its source range before
  writing, so overlapping moves are correct; `fill` reuses `slice_bounds` and
  returns the receiver; the find family follows the existing iterator
  convention (callback gets element/index/thisArg, `this` is the global
  object). The census confirms the win: nextjs members 67 -> 61 with `at`,
  `copyWithin`, `fill`, `findIndex`, `flat` and `flatMap` gone and no new
  names.
- **Measured.** Suite **861** default / **865** `engine-b + woff2` (four array
  tests, one parser test). The five older references keep identical geometry
  and CSS counts: nextjs 7578px / 569 runs / 146.0-151.4 MB (two runs), MDN 2773 / 641 / 85.8,
  Wikipedia 24673 / 10951 / 100.9, HN 1695 / 720 / 75.8, rust-lang 5800 / 483
  / 89.0; `[css-miss]` 84/12/29/3/9 unchanged. google.com's baseline: 1400px /
  28 runs / 95.8 MB, `[css-miss]` 50 names (transitions 36, `pointer-events`
  35, animations 30, then `inline-size`/`block-size`/`box-orient`), `[js-miss]`
  20 globals / 11 members (`XMLHttpRequest`, `trustedTypes`, `fonts.timing`,
  `sendBeacon`, ...), 7 script errors (Google's closure bundle hits parser
  limits: `try/catch` with `yield` in a generator and one comma-expression
  syntax error).

### Wave 76 findings

- **`Reflect` is glue, not a subsystem.** Every method already had an
  implementation: the operators (`get`/`set`/`delete`), the `Object` static
  arms, the prototype walk, and the `new` path. The wave is routing plus one
  extraction (`construct` out of `ExprKind::New`) and one unification (`in`'s
  per-type lookup became `has_member`, shared with `Reflect.has`). A test
  asserts `'x' in o` and `Reflect.has(o, 'x')` together so the two surfaces
  cannot disagree later.
- **`construct` had to be shared, not copied.** `Reflect.construct` must
  behave like `new`: a fresh `this` from `target.prototype`, `new.target` set
  (`pending_construct`, which class field initializers and brand checks read),
  and a returned object overriding `this`. Copying the arm would have
  duplicated all three rules; extracting it left `ExprKind::New` one line.
  The optional `newTarget` argument is ignored — documented, since nothing on
  the references passes it.
- **A built-in object's missing members read as `member`, correctly.** The
  census classifies by the *base*, so `Reflect.defineMetadata` (undefined on
  an object full of natives) lands in the member bucket, not the global one.
  That is why nextjs members rose 58 → 67 while globals fell 61 → 60: the new
  `Reflect` object let core-js's decorator-metadata module run its probes for
  the first time (`defineMetadata`, `deleteMetadata`, `getMetadata`,
  `getMetadataKeys`, `getOwnMetadata`, `getOwnMetadataKeys`, `hasMetadata`,
  `hasOwnMetadata`, `metadata` — one read each). Nothing regressed; the queue
  got one layer deeper, which is exactly what the instrumentation is for.
- **The lenient/strict split is deliberate.** `Object.freeze`/`seal` are
  identity and `Object.isFrozen`/`isSealed` say false for objects, so there
  are no integrity levels to consult: `Reflect.isExtensible` is always `true`
  and `Reflect.preventExtensions` reports success without tracking anything.
  `Object.*` stay lenient (they return the target), while `Reflect.*` return
  booleans and throw TypeError on a non-object where the spec does, because
  callers branch on the return value.
- **Measured.** Geometry and run counts are identical to wave 75 on all five
  references (nextjs 7578/569, MDN 2773/641, Wikipedia 24673/10951, HN
  1695/720, rust-lang 5800/483) and the CSS census is unchanged
  (84/12/29/3/9). RAM stayed in its usual noise band: nextjs 146.7 vs 146.4
  MB, Wikipedia 101.2 vs 101.0, HN 76.0, rust-lang 82.5, and MDN measured
  92.7 MB on the first run and 75.3 MB on a second — the page's own script
  timing, not the wave. Suite **856** default / **860** `engine-b + woff2`.

### Wave 75 findings

- **Two buckets because `undefined` means two different things.** An
  undefined read on a plain script object is ordinary JavaScript — `if
  (!obj.foo)`, optional chains and `typeof` guards fire constantly — so
  recording it would drown the census in noise no engine change could fix.
  `miss_bucket` therefore counts only the global object (a name that
  resolved nowhere) and objects the engine built: an own `__node` marker,
  any native method/accessor (the singleton wrappers are built from natives
  alone, and native-backed built-ins classify the same way), or a
  `Live`/`Style` kind. Style objects answer upstream through `style_get`
  and never reach the hook — their arm in `miss_bucket` is defensive. The
  `undefined` name and `#`-prefixed internal slots are excluded at record
  time, so neither bucket can report them.
- **Feature detection is inherently ambiguous, so rank beats presence.**
  nextjs's global bucket is mostly an environment-sniffing bundle:
  `process` ×6, `Reflect` ×5, `__REACT_DEVTOOLS_GLOBAL_HOOK__` ×3,
  `reportError` ×3, `Deno`/`Bun`/`ActiveXObject`, `MessageChannel`/
  `setImmediate` task-queue shims and ~30 DOM constructor names ×1 each
  (`NodeList`, `NamedNodeMap`, `IntersectionObserver`, …) — probes that are
  *meant* to fail on a browser. Frequency is what separates those from real
  gaps: `Reflect` ×5 is a genuine missing global, `NodeList` ×1 is a probe.
- **The top member reads are page-built keys, not implementable gaps.**
  nextjs's highest-ranked member names are
  `Symbol(Symbol.iterator)_6.654735974936529` (×8), `@@iterator` (×4) and
  `deleteAll` (×4). The odd `_6.65…` keys are constructed at runtime by the
  page — the literal never appears in the cached source — and no engine path
  appends such a suffix (every `format!`/concat site was searched): three
  keys share one fractional part with different integer parts, i.e. one
  page-side float combined with an integer counter, stringified by the
  engine's ordinary number formatting. They are genuine misses (a computed
  key read on a wrapper, resolved to `undefined`) but nothing the engine
  could add is named that, so the actionable queue starts below them.
- **The ranked member list is the new JS queue.** Confirmed absent from the
  source: ES2024 Set methods (`union`/`intersection`/`difference`/
  `isSubsetOf`/`isSupersetOf`/`isDisjointFrom`/`deleteAll`/`addAll`), Map
  helpers (`emplace`/`upsert`), Array extras (`find`/`findIndex`/`flat`/
  `flatMap`/`at`/`copyWithin`/`fill`), `codePointAt`,
  `Object.isExtensible`/`getOwnPropertySymbols`, `Math.clz32`,
  `reportError` and the `Reflect` global. `innerHTML` as a *read* is HN's
  only miss: element wrappers expose only the `setInnerHTML` native, so
  `el.innerHTML` resolves to `undefined`. Array/Set method names read on
  wrappers (`filter`/`reduce`/`some`/`every`/`entries`/`values`) show pages
  treating DOM collections as arrays — the live-collection wrappers are the
  likely base. The other four sites are near-clean: MDN 1/0, Wikipedia 2/1,
  HN 0/1, rust-lang 1/0 (globals/members).
- **Measured, and it costs nothing.** With `MAZDA_MISS=1` all five
  references keep byte-identical geometry and run counts (nextjs 7578/569,
  MDN 2773/641, Wikipedia 24673/10951, HN 1695/720, rust-lang 5800/483) and
  the CSS census is unchanged (84/12/29/3/9), so the hooks moved no
  behavior; RAM is within run-to-run noise (146.4 vs 146 MB on nextjs).
  Suite **854** default / **858** `engine-b + woff2`.

### Wave 74 findings

- **The whole feature is record-gated.** `backdrop-filter` costs pages
  without it nothing: no group recorded → the deferred spans come back
  empty, phase 1 equals the old paint and phase 2 never runs (the render
  test asserts this half explicitly). The five channel loops moved into
  one closure whose only change is an `in_spans(…) != one` guard per loop —
  no other paint behavior moved.
- **Spans are subtree windows, not sorted lists.** The DFS opens each
  group's window before the subtree's pushes and closes it after the
  children recurse, so nested groups produce overlapping spans —
  `backdrop_spans` sorts and merges them so each channel entry defers
  exactly once (a naive per-group skip would defer an entry twice and
  double-blur the region edges). The integer-`z-index` split that forces
  sticky groups to disable themselves disables a backdrop group the same
  way: all spans empty, `blur = 0`, the subtree paints in tree order.
- **Three box passes ≈ the text-shadow recipe, again.** The spec's blur
  radius is σ, and a single box pass at that radius comes out about twice
  too sharp, so `blur_backdrops` reuses the three-pass `box_blur` chain
  (per colour plane, snapshot → blur → rounded-corner write-back) that
  `text-shadow` already validated. Corner culling copies `paint_solid_rect`'s
  radius math so the frost follows the border box's rounding.
- **Measured.** Local capture shows the frosted nav, the soft stripes
  inside the rounded panel and the sharp edge outside it. Dropped-name
  counts: 84 nextjs.org (was 85 — `backdrop-filter` was the last of its
  misses), 12 MDN, 29 Wikipedia, 3 Hacker News, 9 rust-lang.org; the only
  name-set change vs wave 73 is that removal. References keep identical
  geometry: nextjs 7578px / 569 runs / 146 MB, MDN 2773 / 641 / 84 MB,
  Wikipedia 24673 / 10951 / 101 MB, HN 1695 / 720 / 75 MB, rust-lang 5800
  / 483 / 87 MB; suite **852** default / **856** `engine-b + woff2`.
- **A 90-second reference run is not the recorded reference.** An extended
  Wikipedia run kept going past the usual stabilization window and
  accumulated JS-driven relayouts (351 coalesced scroll events, +7 boxes,
  a late-injected +13-rule stylesheet) until a final load measured
  49243px — page JS mutating a long-lived session, not a layout change:
  the first six loads in that same log all measured the recorded
  24673px / 10951 runs. The recorded numbers still come from the
  standard short run.

### Wave 73 findings

- **The axis is the only thing stored.** `dense` would need a placement
  cursor that backfills holes, but this grid's auto items are always 1×1
  (no `grid-row*` spans exist yet) and named/area placement already tracks
  its own per-column occupancy, so the sparse cursor never skips over a
  cell an auto item could use — dense and sparse place identically today.
  Accepting the token and documenting why it isn't stored keeps the parser
  honest (sites emit `grid-auto-flow: dense` 6× in the CSS cache) without
  a second cursor for zero behavioral difference.
- **Two counts, because column flow grows the grid sideways.** Row flow
  wraps at the column count, so one number (`nc0`) served both the cursor
  and track sizing. Column flow wraps at the ROW count and creates
  implicit columns, so `nc` is re-derived from the placed spots after the
  loop and `tracks` is resized with `Track::Auto`. The row-flow path is
  arithmetically unchanged (`max c1 ≤ nc0` there), which is why all five
  references stayed byte-identical — the resize guard `!tracks.is_empty()`
  also preserves the single-full-width-track fallback when nothing places.
- **Reference runs must match the recorded URLs.** The first verification
  pass used `nextjs.org/docs/app`, the MDN `grid-auto-flow` article and
  `/wiki/Web_browser`, and got wildly different heights (2030/5539/7329)
  — the wave table's numbers are for `nextjs.org/`, the MDN homepage and
  `/wiki/JavaScript`. Worth a note: comparing runs across different URLs
  looks exactly like a layout regression.
- **Measured.** Local capture shows all five grid cases in order.
  Dropped-name counts: 85 nextjs.org (was 86), 12 MDN (13), 29
  Wikipedia, 3 Hacker News, 9 rust-lang.org — zero `grid-auto-flow` misses
  remain. References keep identical geometry: nextjs 7578px / 569 runs /
  147 MB, MDN 2773 / 641 / 85 MB, Wikipedia 24673 / 10951 / 102 MB, HN
  1695 / 720 / 76 MB, rust-lang 5800 / 483 / 83 MB; suite **848** default
  / **852** `engine-b + woff2`.

### Wave 72 findings

- **Two layers, because `Style` has to stay `Copy`.** `text-shadow` is an
  inherited text property, so it belongs on `Style` — but `Style` is `Copy`
  and is stored on every box and every run, so a `Vec` was never an option.
  `[Option<TextShadow>; 2]` keeps the copy, costs ~40 bytes per style with
  no allocation, and caps the layer count honestly (documented on
  `TextShadow` rather than silently truncating). Parsing reuses the
  `box-shadow` tokenizer — the grammar is the same minus spread — so a new
  value parser was 15 lines of mapping, not a second tokenizer.
- **One plane per run, not one pass per glyph.** The shadow rasterizes the
  run's glyph masks into a single coverage plane, blurs the plane, then
  composites it per layer. Blurring per glyph would either halo each glyph
  separately (wrong: the shadow must merge across glyphs) or re-walk the run
  per layer. The walk itself moved into `for_each_glyph`, shared with
  `draw_text`: the letter/word-spacing arithmetic has a standing COUPLING
  comment saying it must match layout's measured width exactly, and a
  second copy of it is exactly how that breaks.
- **Blur is three box passes ≈ a Gaussian with σ = blur/2** — the CSS
  definition of the blur radius, so `text-shadow: 0 1px 4px` lands at the
  same softness as the box-shadow pass. The plane is clamped to the buffer
  before allocation, so an off-screen run never allocates for pixels that
  cannot be drawn, and offsets are rounded to whole pixels (no sub-pixel
  shadow position — documented).
- **Paint order falls out of the existing run loop.** The shadow is emitted
  at the top of `draw_text`, so it is under its own glyphs, and because runs
  paint in list order a later run's shadow paints over an earlier run's
  text — which is what in-flow CSS order does anyway. Layer 0 composites
  last (CSS: first layer on top). The underline/strike/overline decorations
  are NOT shadowed; they ride a separate rect channel and would need the
  same treatment in a follow-up.
- **Measured.** Local page: hard red offset, blurred blue glow, two stacked
  layers (green at +3/+3, magenta at +9/+9) and a `currentcolor` glow all
  render, and a unit test pins the hard shadow's pixel set to exactly the
  ink translated by (dx, dy) plus the ink itself. Dropped-name counts: 86
  on nextjs.org (was 87), 13 MDN, 29 Wikipedia, 3 Hacker News, 9
  rust-lang.org. References keep identical geometry: nextjs 7578px / 569
  runs / 147 MB, MDN 2773 / 641 / 84 MB, Wikipedia 24673 / 10951 / 101 MB,
  HN 1695 / 720 / 76 MB, rust-lang 5800 / 483 / 83 MB; suite **844**
  default / **848** `engine-b + woff2`. The +40 bytes per Style show up as
  ~1 MB on Wikipedia's 11k runs, inside the ±10 MB run-to-run spread.
- **Next in the queue.** `backdrop-filter` (needs the offscreen group wave
  69 skipped), `grid-auto-flow`, `hyphens`; transitions only if animation
  is ever required.

### Wave 71 findings

- **`background-clip` is three boxes, not a paint feature.** The colour and
  gradient layers already paint through one rect each, so the wave is a
  `BgClip` enum on `BoxStyle` and one `bg_box` computation in `collect_paint`
  (border box → padding box → content box). The background IMAGE rides the
  placeholder child, which is still sized/positioned against the padding box
  (so `%` sizes and offsets resolve where CSS says) and gets cropped by
  passing the content window down to that child instead of changing its
  geometry.
- **The rounded ring needs a second inset.** With a border and a radius the
  box already paints a ring: the border colour fills the border box and the
  colour layer paints inside it at the padding box. `content-box` moves that
  inner rect to the content box and shrinks the radius by border + padding
  (CSS shrinks radii per side; this uses the max inset — a documented
  approximation). Without a ring the clip box is used directly, so
  `padding-box` on a square box is one fewer rect than before, not a repaint.
- **`text` is deliberately not accepted.** `background-clip: text` needs the
  background painted only inside the glyphs; the renderer has no
  text-shaped mask for a whole layer. Accepting it would silently paint a
  rectangle behind invisible text, so it stays a dropped declaration (the
  wave-69 `filter` doc pattern), and `BgClip` says why where the next reader
  looks.
- **The `background` shorthand resets it.** CSS resets every background-*
  longhand the shorthand omits; the arm now sets `bg_clip` back to
  `border-box` alongside repeat/position/size, otherwise a later
  `background: red` would keep a stale `content-box`.
- **Measured.** Local page: a 100px box with 6px transparent border and 10px
  padding paints its colour at 100 wide (border box), 88 at x = 6
  (`padding-box`), 68 at x = 16 (`content-box`), and the gradient follows
  the same clip (sentinel rect at 88 wide). Unit tests pin the parse (three
  values, `text`/bogus dropped, shorthand reset) and the three rect widths.
  Dropped-name counts: 87 on nextjs.org (was 88, `background-clip` gone),
  13 MDN, 29 Wikipedia, 3 Hacker News, 9 rust-lang.org. References keep
  identical geometry: nextjs 7578px / 569 runs / 147 MB, MDN 2773 / 641 /
  84 MB, Wikipedia 24673 / 10951 / 100 MB, HN 1695 / 720 / 76 MB, rust-lang
  5800 / 483 / 82 MB; suite **842** default / **846** `engine-b + woff2`.
- **Next in the queue.** `backdrop-filter` (needs the offscreen group wave
  69 skipped), `text-shadow`, `grid-auto-flow`; transitions only if
  animation is ever required.

### Wave 70 findings

- **The shorthand is only worth adding when the longhands already exist.**
  `place-items` and `place-self` introduce no layout code: they parse into
  `align` + `justify_items` and `align_self` + `justify_self`, which grid and
  flex already consume. The wave is therefore two parse arms, one shared
  keyword parser, and no new box fields, so RAM is unchanged.
- **One value means both axes; a bad second value means no declaration.**
  `place-items: center` centers both axes, while `place-items: center bogus`
  keeps the element's earlier values rather than half-applying the first
  token. Both helpers return `Option` for the whole value and the apply arm
  writes only inside `Some`, so the invalid-at-computed-value-time rule falls
  out of the shape. `place-content` predates this and still falls back to
  start per axis; it is left as is.
- **`auto` is a value, not an error.** `place-self: auto` is valid and means
  "inherit the container's alignment" on both axes, which is exactly the
  `None` state of `align_self`/`justify_self`. Treating it as an unknown
  keyword would leave a stale override behind after a later `auto`.
- **Verifying it uncovered a grid alignment bug.** The grid row pass shifted
  items by `bx.align` only, ignoring `align_self`; flex already did
  `align_self.unwrap_or(align)`. So `place-self: end start` on a grid item
  moved it horizontally but not vertically. Grid now resolves the per-item
  value the same way flex does, and a unit test pins `None` / `Center` /
  `End` to y = 0 / 45 / 90 inside a 100px row.
- **Auto rows do not stretch, so the check needs a definite row.** With
  `min-height: 120px` and no `grid-template-rows`, the row stays 20px (the
  content height) and `align-items: center` has no slack to distribute -
  the documented shortcut that `stretch` packs auto rows at the start. The
  local page therefore uses `grid-template-rows: 100px`: two 20px items in
  100px cells land at x = 40/140 and y = 40 with `place-items: center`,
  x = 80/180 and y = 0 with `start end`, a `place-self: end start` item at
  x = 0 and y = 80, and the flex row centers at y = 40. `MAZDA_MISS` reports
  no `place-*` names left.
- **Next in the queue.** `backdrop-filter` (needs the offscreen group wave
  69 skipped), `text-shadow`, `background-clip`, `grid-auto-flow`;
  transitions only if animation is ever required.

### Wave 69 findings

- **Collapse the list at parse time.** A filter chain is a composition of
  affine RGB maps interleaved with alpha multipliers, and that family is
  closed under composition. `parse_filter` folds the whole value into one
  `Filter { m: [f32; 9], off: [f32; 3], alpha: f32 }`, so runtime application
  is nine multiplies, three adds and one alpha scale per colour - no function
  list, no per-pixel branch. It also makes `stops` filtering exact: an affine
  map commutes with the linear interpolation a gradient performs, so
  filtering each stop is the same as filtering the ramp.
- **One effective state, two channels.** `apply_effects` (renamed from
  `apply_opacity`) walks the tree and composes `own.then(ancestor)`: a child's
  filter runs before its parent's. RGB maps multiply that way; alpha does not
  compose the same way, because the ancestor's alpha is already owed to the
  child's output. So each box folds only its own filter alpha into `n.eff`
  and stores the accumulated map with alpha normalised back to 1 for its
  children. Applying the map and then `n.eff` once per primitive is what keeps
  nested filters from double-darkening.
- **Filtered inlines must become boxes.** The painter draws plain inline text
  directly from the text leaf, with no box of its own to hang a filter on.
  `paints_chrome` now returns true when `bx.filter` is set, reusing the same
  promotion path as backgrounds and borders. Without it, the common
  icon-filter pattern on a bare `<span>` silently drops the filter.
- **Per primitive is honest but not a real group.** Colours, gradients,
  images and text each pass through the filter, but there is no offscreen
  composite, so overlapping children of a filtered parent blend with each other
  instead of being filtered as one flattened image. `blur()` and
  `drop-shadow()` need that buffer and stay no-ops; `backdrop-filter` is not
  modelled. This is recorded as a `shortcut:` comment.
- **Measured.** Local page: inverted text paints cyan (`0x00ffff`), grayscale
  black is `0x363636`, `brightness(0) invert(1)` is white, `hue-rotate` gives
  `0x006d6d`, a filtered gradient runs cyan to yellow, `opacity(0.5)` over
  black packs as `0x80000000`, nested filters compose, and a filtered PNG is
  cyan; `MAZDA_MISS` reports zero dropped `filter` names. Dropped-name totals:
  89 (nextjs.org), 15 (MDN), 29 (Wikipedia), 3 (Hacker News), 9
  (rust-lang.org). All five reference heights and run counts are unchanged.
  Suite **838** default / **842** `engine-b + woff2`.
- **Next in the queue.** `backdrop-filter` (needs the offscreen group this
  wave skipped), `text-shadow`, `background-clip`, `place-items`/`place-self`;
  transitions only if animation is ever required.

### Wave 68 findings

- **A shorthand must reset, so it parses into its own struct first.** `font`
  sets five longhands at once and clears the ones it omits; applying pieces
  as they are seen would leave a stale `bold`/`italic`/`line-height` behind on
  a partial value. `parse_font_shorthand` returns `Some(FontSh)` only for a
  complete value, and the apply arm touches the style inside that `Some`. A
  missing family, an unknown pre-size token, a system keyword
  (`caption`/`menu`/…) and `inherit`/`unset` therefore change nothing - the
  spec's "invalid shorthand is ignored" behaviour falls out of the shape.
  `font: initial` is the one keyword that resets everything (16px, normal
  weight/style, default family, default line-height).
- **The bare number is the ambiguity.** `font: 700 16px serif` must read 700
  as a weight, but `parse_size` accepts a bare number (font-size has no
  unitless form, but it does allow `0`). The size slot is recognised
  syntactically instead: only a value with a unit, `%`, a size keyword or
  math function (or the literal `0`) is tried as a size; a bare 1-1000 then
  becomes a weight. `font: 700 16px` with no family is dropped whole.
- **The family stays interned.** `fam_stack_of` gained the `font` case and
  runs the same splitter to find the family tail, so the shorthand reuses the
  existing `StackTable` ids and font picker. That parses the value twice per
  declaration (once for the id, once to apply); at 80 declarations on MDN it
  is not worth a second IR.
- **The line-height helper paid a bonus.** Moving `line-height` onto
  `parse_line_height` extended it from unitless/px/% to em/rem/vw/`normal`.
  Wikipedia's `line-height: 1.4em` and `1.6em` rules now resolve, so its
  reference height legitimately changes 24575 → 24673px (same content, the
  line boxes grow). This is a behaviour change to re-baseline, not a
  regression; the other four reference heights are unchanged.
- **Measured.** Local page: a 24px/1.5 box is 36px tall, `700 1.2rem` is
  19.2px with line-height reset to normal, `2vw/2` is 20px with a 40px line,
  `oblique 10deg` is accepted italic, `font:inherit`/`menu`/`700 16px` leave
  the inherited font alone. Dropped-name counts: 90 (nextjs.org), 16 (MDN),
  30 (Wikipedia), 4 (Hacker News), 9 (rust-lang.org). Suite **832** default /
  **836** `engine-b + woff2`.
- **Next in the queue.** `filter`/`backdrop-filter`, `text-shadow`,
  `background-clip`, `place-items`/`place-self`; transitions only if
  animation is ever required.

### Wave 67 findings

- **The miss table paid for itself again.** After wave 66 the top visual
  entries were transitions (a deliberate no-animation shortcut), the
  individual `translate` (177 declarations on nextjs.org), `rotate` (45) and
  `scale` (36) properties. `translate` and `scale` reuse the existing
  paint-time `Xf` composite, so they cost parse entries plus one box field,
  not a new paint path. `rotate` cannot be faked: rects stay axis-aligned and
  glyph runs stay horizontal, so it is accepted and contributes nothing, which
  is exactly what `rotate()` inside a transform list has always done.
- **Cascade independence needs separate storage.** `translate`, `rotate` and
  `scale` are three properties, not a shorthand for `transform`: a later
  `transform: none` must not clear an earlier `translate`. Folding them into
  `BoxStyle::transform` at parse time would make declaration order change the
  result, so `BoxStyle` gains `translate: [Len; 2]` and `scale: [f32; 2]`
  with zero/one as the identity defaults (no "declared" flag needed). The cost
  is 32 bytes per box: nextjs.org's 149.4 MB is about 0.5 MB above wave 66,
  and the other references moved by at most 0.4 MB (RAM is noisy at this
  scale).
- **Composition order is the spec order.** The list applies innermost
  (`translate -> rotate -> scale -> transform`), so the effective map is
  `sx = scale.x * transform.sx`, `tx = scale.x * transform.tx + translate.x`
  (likewise y), then the existing origin correction
  `x + (n.x + ox)(1 - sx)`. A lone `translate` value leaves y at 0 and a lone
  `scale` value scales both axes; `%` translate resolves against the box's own
  w/h at paint, like the list form.
- **Verifying the paint found a clipping bug.** The solid-rect fill computed
  its start column as `sx.max(0.0)` but its end as `start + w`, so a rect at
  `x = -50` painted 200 columns from x = 0 instead of 150. `transform:
  scale(2)` already hit this (a left-edge box spans -50..150); with the
  `scale` longhand it becomes routine, so the fill moved into one
  `paint_solid_rect` helper that intersects the span with the buffer, with the
  rounded-corner path and the underline/caret rules sharing it. Gradients were
  already clipped per pixel and are unchanged. A unit test pins the
  negative-origin intersection and the fully-off-buffer case.
- **Measured.** Local page: 20/10px translate, `scale: 2` about the center
  (painted 200x80 at -50, 40 and intersected to x <= 149 at the page edge),
  `rotate: 45deg` unrotated, zero dropped names. References keep their exact
  height/run counts; dropped-name counts are 91 (nextjs.org), 31 (Wikipedia),
  17 (MDN), 4 (Hacker News), 10 (rust-lang.org). Suite **828** default /
  **832** `engine-b + woff2`.
- **Next in the queue.** The `font` shorthand (80 declarations on MDN is the
  largest remaining visual miss), `filter`/`backdrop-filter`, `text-shadow`,
  `background-clip`, `place-items`/`place-self`, then transitions only if
  animation is ever required.

### Wave 66 findings

- **Instrument the gate, not the renderer.** Every stylesheet rule and inline
  style already passes through one `KNOWN` check, so one counter there sees
  every property the engine silently drops. The switch is a `OnceLock` env
  read; when `MAZDA_MISS` is unset each declaration pays one atomic load and
  allocates nothing. When set, a mutex-guarded `HashMap` counts names and
  `dump_misses()` prints them sorted by frequency, then name, after each worker
  load (cumulative, so the last table before exit is the session total).
- **At-rules are counted in the same table.** `handle_at_rule` already knows
  which names it implements (`media`, `supports`, `layer`) and which it skips
  by documented shortcut (`import`, `font-face`, `keyframes`); every other name
  is a genuine miss. The references report `@property` (98 occurrences on
  nextjs.org, 21 on MDN), `@container` (21) and `@starting-style` (1), plus
  Wikipedia's 3 `@counter-style` blocks.
- **The ranking changes the work queue.** Ignoring behavioural properties
  (`user-select`, `pointer-events`, `touch-action`) and the deliberate
  no-animation shortcut, the visual misses were individual transforms
  (`translate` 59, `rotate` 15, `scale` 12 on nextjs.org), `filter` /
  `backdrop-filter` (11+10), `text-shadow` (6), `background-clip` (6) and the
  `font` shorthand (8 on MDN, 6 on rust-lang.org). Grid shorthand
  `place-items`/`place-self` appear on MDN (4+1) and nextjs.org (1+0).
- **Borders were the cheapest real win.** `BoxStyle.border` is already
  `[Len; 4]`, so per-side widths needed parse entries, not layout work. The
  `KNOWN` table had `border-width` but not `border-top-width`, and
  `border-radius` but none of the four corner longhands (the handler had a
  top-left arm, unreachable through the gate). Adding the four widths, four
  colours and four corner radii makes resets and utility classes that split
  the shorthand draw their borders.
  A per-side colour still collapses to the model's single `bcolor` (last one
  wins), and every corner longhand sets the single `radius`; both are the
  pre-existing single-value shortcuts, now documented at the parse site.
- **Style longhands are accepted no-ops.** `border-style` and its four
  longhands now survive the gate but change nothing, exactly like the style
  words inside `border`/`border-top` have always been ignored (every border
  paints solid). That keeps a style-only declaration from looking like a
  missing parse entry without inventing dashed-border paint.
- **Measured.** Local page: 6px top / 10px left ring at the expected pixels,
  10px border inner edge at x=10, radius-12 corners leave the outer corner
  white and radius-20 clips the second box; `MAZDA_MISS` reports zero dropped
  names for the page. References keep identical height and run counts (see the
  measurement row) and RAM stays in the usual bands. Distinct drops fall from
  111 to 94 (nextjs.org) and 23 to 10 (rust-lang.org). Suite **825** default /
  **829** `engine-b + woff2`.
- **Next in the queue.** Individual `translate`/`rotate`/`scale` (they fit the
  existing transform-composite path), the `font` shorthand, `place-items` /
  `place-self`, `filter`/`backdrop-filter`, `text-shadow`, `background-clip`,
  then `@container`/`@property` if a reference starts depending on them.

### Wave 65 findings

- **Per-primitive alpha, not a group composite.** A real `opacity` renders the
  element and its descendants into an offscreen group and composites the group
  once. This wave instead folds the effective alpha into each primitive the box
  paints (backgrounds, borders, outlines, shadows, gradients, images, text runs
  and decorations). A single translucent layer is exact and costs no extra
  buffer; the documented shortcut is that overlapping siblings inside a
  translucent parent can show through each other.
- **Alpha rides the colour word's unused high byte.** Every paint channel
  already carried `0x00RRGGBB`, so `with_alpha()` packs a 1..255 alpha into the
  high byte and the paint loop reads 0 as "opaque". That kept the
  colour-carrying channels (rects, underlines, shadows) byte-compatible and
  left every fully opaque primitive byte-identical to the pre-wave snapshots.
- **Compose once at build time.** `apply_opacity()` walks the box tree after
  `build_into` and stores `eff = parent * own` on each box, so nested fades
  multiply without per-frame work. `opacity: 0` is still turned into
  `display:none` during the cascade, which keeps fully transparent subtrees out
  of layout and paint entirely.
- **Text needed the value twice.** `layout_from` splits atoms into words, so the
  effective opacity is threaded as `Atom.eff` through `word`, `push_words` and
  `push_preserved`; the emitted `PositionedRun` carries it as a `u8` alpha for
  glyph coverage, while underline/strike/overline colours are alpha-packed at
  emission (otherwise a faded decoration would stay fully opaque).
- **Measured.** The local page verifies the blend math: 50% red over white is
  `255,127,127`, 25% black is `191,191,191`, nested 50% red over green is
  `128,127,0`, and 50% black ink lands at grey 137. The five references kept
  their exact geometry at 1000x616 and stayed in their RAM noise bands:
  nextjs 7578px / 569 runs / 148.8 MB, MDN 2773 / 641 / 89.2 MB, Wikipedia
  24575 / 10950 / 103.0 MB, Hacker News 1695 / 728 / 78.2 MB, rust-lang
  5800 / 483 / 84.7 MB.
- **Cost.** One `f32` per `BoxStyle` (declared value) and per `BoxNode`
  (effective value), plus one alpha byte on the run/image/wire entries. No new
  allocation per primitive and no extra per-frame buffer.

### Wave 64 findings

- **One inline expression set the size of every statement.** `StmtKind` was
  176 B because `For` inlined two 80 B expressions while `If`, `Switch` and
  `DefineAcc` each inlined one. Boxing those fields leaves a 56 B enum, so
  expression statements, declarations, blocks and every other common
  statement save 120 B each. The box is allocated only for a statement that
  actually carries the expression.
- **Box the rare per-statement payloads, not the statement.** Boxing the
  whole `StmtKind` would have moved the same bytes to the heap and added an
  allocation per statement; boxing only the large fields keeps the common
  variants compact in the enclosing `Vec<Stmt>`.
- **`try`'s catch clause was the next-largest variant.** `CatchClause`
  (name plus a statement vector) is 32 B and appears once per `try`, so
  boxing it brings `StmtKind` down to the 56 B `Export`/`DefineAcc` range.
- **Only arrows need captured `this`.** `Closure.this` had exactly one read
  (the arrow-call path) and every non-arrow constructor wrote
  `Val::Undefined` into a 24 B slot. `Option<Box<Val>>` makes normal closures
  pay 8 B and arrows allocate their captured value once.
- **Measured.** `StmtKind` 176 -> 56 B, `Stmt` 192 -> 72 B (-62%),
  `Closure` 112 -> 96 B; `Obj` stays 64 B and `Param` 40 B from wave 63.
  nextjs.org's working set fell from 153.2 to 148.2 MB with identical
  7578px / 569-run geometry; MDN, Wikipedia and Hacker News stayed within
  their previous noise bands.
- **Tradeoff.** Loop tests and branch conditions gain one indirection; the
  test suite runtime is unchanged and the common statement path is smaller
  and more cache-friendly. Normal closures lose no behavior because their
  `this` still comes from the call site.

### Wave 63 findings

- **Box the rare payloads, keep the common ones inline.** `ObjKind` is
  embedded in every `Obj`, so its largest variant set the size of all objects.
  `PromiseState`, `IterState` and `TypedArray` are far rarer than plain objects
  and arrays, so moving each behind a `Box` removed their bytes from every
  object. `Array(Vec<Val>)` stays inline because arrays are common and the
  `Vec` header is already only 24 B.
- **One pointer for error text.** An error previously carried two fat
  `Rc<str>` pointers (16 B each) inside the enum. `ErrorData { name, msg }`
  behind one `Rc` turns that into 8 B; the payload is immutable after
  construction, so sharing is safe.
- **Parameters pay only when they use the feature.** `Param` was 144 B because
  it inlined an 80 B `Expr` default and a 32 B `Pat` even though most
  parameters have neither. Boxing both fields makes the common parameter
  40 B; the extra allocation happens only for a function that actually
  declares `= default` or a destructuring pattern.
- **Measured.** `ObjKind` 56 -> 32 B, `Obj` 88 -> 64 B (-27%), `Param`
  144 -> 40 B (-72%); `Val` stays 24 B. Live working set fell on the
  script-heavy references (nextjs 155.7 -> 153.2 MB, MDN 89.8 -> 87.3 MB)
  while geometry and run counts stayed identical. Hacker News ranges
  67-83 MB across reloads, so only its stable 1695px / 728-run geometry is
  meaningful. Wave 44 showed that single-process RSS is noisy; the exact
  per-unit sizes are the deterministic measure.
- **Tradeoff.** Errors, promises, iterators, typed arrays and parameters with
  defaults/patterns gain one indirection on access. Ordinary object and
  parameter creation gets cheaper and more cache-friendly, which is the path
  React-style code exercises most.
- **Still ahead.** `Stmt` (192 B) and `Closure` (112 B) are untouched; a later
  wave can box their large variants the same way.

### Wave 62 findings

- **Geometry resolves at paint time.** The centre is stored as the same
  symbolic `BgPos` pair that `background-position` uses, so percentage centres
  follow the box's painted size without a layout pass and the radial path
  needs no new rect or layout field. `radial_radii` turns centre, shape and
  size into `(cx, cy, rx, ry)` once per gradient and the per-pixel loop only
  does the normalized-distance test.
- **The linear path stays byte-identical.** `Gradient` gained one
  `Option<Radial>` and a `None` value means linear; `dir` keeps its initial
  value for radial gradients. The per-pixel branch picks the radial metric or
  the old projection, so stored linear gradients, their tests and measured
  geometry are untouched.
- **A single normalized metric covers every shape.** `t = sqrt(...)` is 0 at
  the centre and 1 on the ending shape, and the shared stop interpolation
  clamps outside it, so the last stop fills the corners and everything past
  the shape. Corner keywords pick the chosen corner's `|dx|`/`|dy|` as the
  ellipse radii so `t` reaches exactly 1 there; a circle's percentage radius
  resolves against `hypot(w, h) / sqrt(2)`, and a degenerate axis falls back to
  1.0 to keep the metric finite.
- **Shortcuts.** Corner-keyword ellipses use the corner offsets directly
  instead of the spec's aspect-ratio scaling, `repeating-*` still paints one
  pass, only the first gradient layer is used, there is no
  `background-origin`/`background-clip`, and a gradient whose stops use a
  named colour outside the 16-entry table is dropped whole (the pre-existing
  linear behaviour, surfaced by a local `gold`/`crimson` case).
- **Geometry is untouched.** None of the five references changes height, run
  count or error output, and the only new memory is one `Option<Radial>` per
  gradient-bearing box.
- **Test coverage.** One parser test (shape, extents and synonyms, explicit
  radii, `at` positions, one-length circle inference, headless default, linear
  staying linear, a positioned first colour stop not being eaten as a head)
  and one paint test (centre = first stop, corner past the radius = last stop,
  off-centre blending).

### Wave 61 findings

- **Lines are run tops, not objects.** The engine has no line-box structure:
  `block()` emits each line's runs at a shared `y`, so the clamp collects the
  distinct non-empty run tops. That costs one scan of the subtree's new runs
  and keeps all existing stored `run_range`s valid (nothing is removed from the
  vector; hidden runs simply carry empty text and zero width).
- **Shrink at the cut, not at the last run's bottom.** The first removed line's
  top is exactly the kept line's bottom, including any extra leading from a
  taller inline box, so `n.h = next_top - content_top` needs no line-height
  bookkeeping and mixed font sizes still stack correctly. The box remains a
  normal block, so the next sibling moves up by the removed height.
- **The marker shares the overflow path.** The last kept run is trimmed with
  the same `fit_prefix` + spacing rule as `text-overflow: ellipsis`, measured
  against the box's content edge, so the marker never paints outside the box.
  Underlines at or below the next line's top are zero-sized rather than
  removed, for the same index-stability reason.
- **Shortcuts.** The classic author recipe sets a vendor-prefixed
  `display: box`; this engine treats that spelling as an unknown `display`
  value and keeps block flow, which is enough for the common utility classes and leaves normal block
  layout intact. Nested block text participates in the line count, mixed
  line-heights follow their run tops, only text is truncated (child
  boxes/images below the cut keep their geometry and rely on `overflow`), and
  flex/grid containers skip the clamp entirely.
- **Geometry is untouched.** None of the five references changes height, run
  count or error output; the extra field is one `Option<u32>` per box.
- **Test coverage.** One parser test (plain and prefixed spellings, `none`,
  `0` and garbage clearing, later-declaration wins) and one layout test (three
  wrapped lines clamped to two: height, surviving line count, trailing marker,
  hidden tail).

### Wave 60 findings

- **No new paint channel.** A gradient is just a rect with a sentinel colour plus
  one parallel `Layout::rect_grad` slot. The z-sort reorders the slots with the
  same stable keys, and `StickyGroup.rects` already spans the shared vector, so
  sticky pinning, transforms, overflow clips and parent-before-child order all
  work without new bookkeeping. The alternative (a separate `gradients`
  vector) would have needed its own sort, sticky spans and paint interleaving.
- **Parse at declaration time, keep the value small.** A `Gradient` is a unit
  direction plus `GradStop { pos, color, alpha }`; colors reuse
  `parse_color_alpha`, so `transparent` and `rgba()` keep their real alpha.
  Stops are made non-decreasing, the first/last default to 0/1 and interior
  gaps are distributed evenly. Function-name matching is a substring search, so
  prefixed spellings land in the same parser without writing a prefix.
- **The box background stays underneath.** Alpha gradients blend per pixel over
  whatever is already painted, which is why the existing background-color rect
  is pushed first; a gradient with no base colour therefore shows the parent's
  paint through its transparent stops.
- **Directions are screen-space.** `0deg` = up, `90deg` = right, so
  `to bottom`/`180deg` is `(0, 1)`. Corner keywords normalise the corner vector;
  the real spec's aspect-ratio-dependent corner angle is a documented shortcut.
  `repeating-*` currently draws one pass, and only the first layer of a
  multi-layer background participates.
- **Geometry is untouched.** Gradients are paint-only; all five reference
  heights and run counts are identical to wave 59. The sub-megabyte RAM change
  is the `Arc`'d stop arrays and the gradient slots.
- **Test coverage.** One parser test (directions, angles, stops, alpha,
  `none`/`url` clearing), one layout test (gradient slot rides the rect channel,
  sentinel colour, width), and two paint tests (linear interpolation/clamping
  and left-to-right / top-to-bottom rasterisation). The empty-`height` local
  page pitfall is already documented: a plain block's `height` is ignored unless
  it has content, `min-height` or a replaced/aspect/grid box.

### Wave 59 findings

- **Live means re-query, not caching.** A collection stores only its query
  (family + scope + argument) in a new `ObjKind::Live`; every `length`,
  index, `item`, `namedItem` or named read re-runs it against the current
  arena. That is O(nodes) per read, but collections are read a handful of
  times in practice and the alternative (a cache plus a mutation counter)
  would be a second source of truth.
- **The engine still knows nothing about the DOM.** `js.rs` defines the
  opaque `LiveSpec` and two embedder hooks (`live_get` for member reads,
  `live_values` for iteration). The family numbers and tag lists live in
  `script.rs`. Shorthand: `live_values` snapshots the collection, so
  `for..of`/spread are one query rather than one per index.
- **Array interop is kept on purpose.** Real `HTMLCollection` has no array
  methods, but real pages call generic ones: MDN's anchor code does
  `Array.prototype.filter.call(document.getElementsByClassName(...), fn)`.
  `need_array` now turns a live collection into a one-shot array for those
  methods; writes land in that snapshot, not the DOM. Direct `forEach`/`map`
  on a collection stay absent (browsers do not have them either).
- **`form.elements` is a getter, not a method.** Installing it as a native
  value made `form.elements.length` read the function's arity (undefined) -
  the wrapper now installs an accessor pair, like `parentElement`. Named
  shorthand on the form itself (`form.email`) is still not attached; use
  `form.elements`.
- **The wave found a real ordering bug.** Element-scoped subtree walks
  (`getElementsBy*` on an element, element `querySelectorAll`) popped the
  initial children from the back of a stack without reversing them, so
  siblings came out in reverse document order. Whole-document queries used
  arena order and were fine. Both helpers now push the initial children
  reversed; a new assertion pins `form.elements.item(1)` to the second
  control in document order.
- **`querySelectorAll` stayed static.** Only `getElementsBy*`, `forms` and
  `form.elements` became live, matching the NodeList/HTMLCollection split.

### Wave 58 findings

- **Captions are placed, not re-gridded.** `table_layout` already painted
  captions before the row loop; bottom captions are the same `place()` call
  moved after the final row and its bottom border-spacing. Cell columns,
  intrinsic widths and row heights are unaffected, so no table measurement
  changed.
- **The inheritance shortcut is resolved at layout time.** `BoxStyle` is
  computed per element, so the table's `caption-side` cannot flow through the
  normal cascade into its caption. A caption's own declaration wins; otherwise
  layout falls back to the table's value, then to `top`. That covers both
  `table { caption-side: bottom }` and `caption { caption-side: bottom }`.
- **Only the block axis is modeled.** `top`/`bottom` and their logical
  `block-start`/`block-end` aliases move the caption; horizontal
  `inline-start`/`inline-end` are dropped rather than guessed. A table with
  mixed-side captions keeps document order inside each side.
- **Total table height is unchanged.** Moving a caption from above to below
  only changes where its vertical advance lands; a unit test asserts the two
  layouts have the same table height, and all five references stayed
  geometry-identical.

### Wave 57 findings

- **Grid-only, and off by default.** `justify-items`/`justify-self` act in
  the grid row loop; flex containers ignore them, as CSS does. The initial
  value is `stretch`, which is exactly the old "pass the whole area" path, so
  no reference page changes unless it authors the property.
- **Non-stretch is fit-content, not re-flow.** The item's outer width becomes
  its max-content width (or its declared width) capped by the grid area, then
  the margin box shifts by the leftover space. A text item is laid out at
  that narrower width, so it wraps like a shrink-to-fit box; a block item
  keeps its internal layout and just moves.
- **Percentages resolve against the area.** A `%` width on a grid item must
  use the grid area, not the whole grid, so alignment has its own
  `grid_item_outer_w` measurement instead of reusing `item_max_content`
  (which sizes `%` widths intrinsically). `min-width`/`max-width` clamp the
  aligned size, matching `place()`.
- **`normal` and `auto` mean different things.** `justify-items: normal` (and
  `justify-self: normal`) is `stretch`; `justify-self: auto` inherits the
  container's value. Unknown keywords (including `legacy`) are dropped and
  keep the previous value.
- **Two bytes, no new pass.** `justify_items: Align` plus
  `justify_self: Option<Align>` fits in existing padding; the slot and shift
  are computed where the item is already placed, so column sizing, row
  heights, spans and the wave-56 `align-content` pass are untouched.
- **MDN moved, this wave didn't.** The MDN homepage measured 2773px this
  wave versus 2690px in wave 56. Running the same binary with `justify-self`
  parsing disabled still gives 2773px, so the difference is the live page
  (contributor-spotlight/footer content), not this feature.

### Wave 56 findings

- **Distribution is a post-pass shift, not a re-layout.** Grid and flex line
  positions are decided exactly as before; once the natural stack height and
  the definite height are both known, each row/line's already-painted runs,
  underline rects and subtrees move by one offset. That keeps the memory cost
  at two small per-row/per-line range vectors and touches nothing in the
  measuring path.
- **`stretch` has no growth model.** `align-content: normal` and `stretch`
  both pack at the start, because auto grid rows and flex lines are never
  re-measured to fill the spare space. The property therefore distributes
  existing rows/lines but cannot enlarge them; this is the same honest
  shortcut used for wavy decorations in wave 55.
- **The spare space is the definite height.** `height` and `min-height` are
  read through one helper (`definite_content_h`), resolved in the same
  border-box/content-box terms `place()` uses. `%` values still resolve
  against the box's content width, matching the pre-existing percent-height
  shortcut.
- **Only a declared box can distribute.** With no `height`/`min-height`
  there is no spare space, so the pass is a no-op. Conversely, when
  distribution does move rows, the container's used height becomes the
  definite height so its background/border covers the shifted content; with
  `start` the old content-derived height is kept (this engine still ignores a
  bare `height` on non-grid boxes).
- **Wrapping gates the flex case.** A nowrap flex row has a single line and
  gets nothing from `align-content`; `align-items`/`justify-content` remain
  the properties that move it. Column flex has no wrapping model, so the
  lines case there is untouched.
- **The reference pages never use it.** No cached reference sheet declares
  `align-content`, so every reference height and run count is unchanged; the
  feature is a conformance win for authored pages, verified on a local page
  and by six unit assertions.

### Wave 55 findings

- **Decoration is a paint property, not flow.** The four new values only
  affect the underline rects emitted next to a text run; line boxes, run
  widths, wrapping and page height never consult them, so every reference
  height and run count stayed identical.
- **The wire stays solid rectangles.** `underline_rects` remains
  `(x, y, w, h, colour, fixed)`, so `dotted`/`dashed` are expanded at layout
  time into short solid segments and `double` into two rules. That costs one
  rect per dash/dot instead of per glyph; only decorated runs pay, and the
  six references stayed within noise.
- **Thickness is the decoration's width.** `auto`/`from-font` store 0 and
  paint at 1px; a declared length sets the rule height. For `double`, the two
  rules share that declared thickness (half each, at least 1px, with a 1px
  gap) rather than each drawing the full width.
- **Offset is underline-only.** `text-underline-offset` adds to the
  underline's default `line-height - 2px` position; strikethrough and
  overline keep their fixed mid-line/top positions, matching the property's
  name.
- **Wavy is the documented fallback.** There is no wave rasterizer in this
  paint path, so `wavy` draws a solid rule; `text-decoration-style: none` is
  an invalid style value and is dropped rather than clearing the line.
- **No colour inheritance model was added.** `text-decoration-color` defaults
  to the run's own colour and is not inherited, like the existing line flags,
  which keep this a per-run paint override instead of a new cascade channel.

### Wave 54 findings

- **No `viewBox` does not mean "use the output size".** An SVG without a
  `viewBox` still has a fixed user coordinate system: the root `width`/`height`
  (the intrinsic size). The old fallback substituted the requested raster size,
  which made the user-space origin track the display size and cropped anything
  larger than it. The logo only looked right at its native 144px and appeared
  merely small at 200px because the untransformed 144-unit drawing still fit.
- **One fallback for both geometry and percentages.** `root_space` and
  `render` now share `root_vb`: `viewBox` if present, else intrinsic
  `width`/`height`, else the requested size. `state.vp` takes the same values,
  so `%` lengths inside a no-viewBox SVG resolve against the intrinsic box too.
- **Uniform fit preserved.** The existing `xMidYMid meet` rule (min scale +
  centring) still applies to both cases; a no-`viewBox` image whose display
  aspect differs from its intrinsic aspect letterboxes rather than stretching.
  That is the documented rasterizer shortcut, unchanged by this wave.
- **Layout is untouched.** The bug was entirely inside `svg::render`; probe
  already reported the intrinsic size correctly, so box geometry, run counts
  and page heights are identical. Only the pixels inside image boxes change.

### Wave 53 findings

- **Origin is a paint anchor, not geometry.** `box_xf` already folded the
  transform into `p·s + (c(1−s) + d)` for the box centre; the change replaces
  `c` with the resolved origin. Layout never consults the property, so flow
  and sibling positions cannot move.
- **Both axes stay symbolic.** `%`/`em` lengths are kept as [`Len`] because
  the box size is not known at parse time; `resolve_len` turns them into
  border-box pixels in `box_xf`, exactly where `translate` percentages
  already resolve.
- **Keyword grammar.** A single value leaves the other axis at `center`
  (`right` = `100% 50%`, `top` = `50% 0`); with two values a leading
  `top`/`bottom` swaps into the Y slot so `top left` and `bottom right` work
  as authors write them.
- **Glyph size still does not scale.** The engine's documented transform
  shortcut is unchanged: `scale()` scales the anchor and every rect, but glyph
  rasterisation keeps its size (and `rotate`/`skew` remain no-ops), so text
  inside a scaled box can overflow it. Origin only chooses the fixed point.
- **No reference moved.** The five captures are identical; `transform-origin`
  on these pages appears in decorative/motion CSS whose transforms the engine
  does not animate, so the wave is a correctness gain for authored 2D
  transforms rather than a layout reflow.

### Wave 52 findings

- **Paint-only, as CSS defines it.** The ring is emitted next to the border
  strips and never touches layout: neighbours keep their coordinates and an
  outline can overlap them, which is exactly why it is not a `border`.
- **Four strips, no corner rounding.** The ring is the border box expanded by
  `offset + width`, split into top/bottom/left/right rectangles. It therefore
  has square corners even on a rounded box, and a negative `outline-offset`
  pulls it inside the border box (the strips still paint, possibly over the
  background).
- **One appearance for every style.** `dotted`, `dashed`, `double`, `groove`,
  `ridge`, `inset` and `outset` all render as a solid ring; the rasterizer has
  no dash phase or bevel model and inventing one would cost memory for no
  layout gain. `auto` is off because the engine drops the `:focus` rules that
  would ever show the platform ring.
- **Defaults match CSS.** `outline: solid` is 3px currentcolor; `outline:
  thick dashed` is 5px currentcolor; `outline: none`, `initial`, `unset`,
  `revert` and `revert-layer` clear it. A width longhand after the shorthand
  overrides it, and `outline-offset` is independent of declaration order.
  `outline-color: transparent` (the common "reserve a focus ring" reset) keeps
  the ring's parsed geometry but suppresses its pixels.
- **The references needed no tuning**, which confirms outlines are mostly
  focus/`outline: none` reset material on static pages: four of five captures
  are pixel-identical and every height/run count is unchanged. The new
  `outline: none` resets also mean fewer UA rings would appear if focus rules
  existed, but they are dropped before that matters.

### Wave 51 findings

- **One clamp, placed after content layout.** `place()` already computes the
  content height from the chosen layout (block/flex/grid/table/replaced),
  then applies `min-height`, then adds padding/border. `max-height` is a cap
  on that same content height, inserted just before the floor. Nothing about
  child placement changes: content keeps its coordinates and spills past the
  shorter box, exactly as CSS overflows.
- **Clipping stays a separate decision.** A capped box clips only when
  `overflow` is not `visible`; the existing content-clip pass then uses the
  capped `n.h`, so runs and backgrounds below the edge are cut without any
  new bookkeeping. `max-height` alone therefore never hides content.
- **Ordering matches CSS Sizing 3.** The ceiling is applied before the
  `min-height` floor, so `min-height` wins when the two conflict (verified by
  a unit test). The declaration follows `box-sizing` like `height`: border
  box by default, content box under `content-box`.
- **The references needed no tuning.** Four of the five are geometry-
  identical; MDN is the one page whose authored `max-height` was previously
  ignored — its 35rem mandala SVG used to inflate the homepage header. The
  runs are unchanged (only the container is shorter), which is why this is a
  fidelity gain rather than a reflow.
- **Shortcuts, documented here.** `%` `max-height` resolves against the
  containing block width (same as `min-height`), an auto block still ignores
  a plain `height`, there is no scrollbar for a capped `overflow: auto`
  box (the content is clipped, not scrollable), and `max-height` on flex
  lines/grid tracks is only the container cap — no track-level clamping.

### Wave 50 findings

- **`text-overflow` is a box flag, applied by the clip pass.** `BoxStyle`
  gained `ellipsis` (parsed from any `ellipsis` token in the value, so the
  two-value `ellipsis clip` form is accepted). It takes effect only where
  `apply_run_clips` already computes a content window (`overflow` not
  `visible`); a box that only declares `text-overflow` keeps painting the
  full text, which matches the spec's "needs a clipping box".
- **Trimming happens per line after positions are final.** Each run whose
  right edge crosses the content edge is rewritten to the longest prefix
  that still leaves room for "…" (the same `fit_prefix` binary search and
  spacing rule as line layout, so the marker lands exactly inside the
  window). Later runs on that line become empty; runs on other lines and
  runs that already fit are untouched.
- **Runs are never removed.** Other boxes' `run_range` indices point into
  the shared run vector, so a fully hidden run keeps its slot with an empty
  string and zero width (paint draws nothing).
- **The marker is ordinary text.** "…" is appended to the run, inheriting
  its font, colour and letter-spacing and flowing through the existing
  paint path — no special glyph or clip logic. A window too narrow for the
  marker shows nothing rather than a half glyph.
- **Shortcuts, documented here.** The box form only (no inline
  `text-overflow`), no `direction`/RTL handling, no fade, no distinction
  between the `clip` and `fade` second values, and vertical overflow is
  not marked (only the horizontal edge is trimmed). The `:hover` target
  still covers the untruncated run, as before.
- **No geometry change on the references.** Truncation shortens text inside
  an already-clipped window, so heights, run counts and RAM were identical:
  nextjs 7578px / 569 runs / 154.3 MB, MDN 2922px / 641 runs / 87.7 MB,
  Wikipedia 24575px / 10950 runs / 101.8 MB, HN 1695px / 728 runs /
  78.1 MB, rust-lang 5800px / 483 runs / 84.8 MB. A local 160px row with a
  36-character token now dumps `ABCDEFGHIJKLMNOPQRST…` (153px) instead of
  the cut-off full string; a fitting label and a box without `overflow`
  are unchanged.

### Wave 49 findings

- **Two inherited switches, no new storage per box.** `Style` gained `ow`
  (`overflow-wrap` / legacy `word-wrap`) and `wb` (`word-break`); both ride
  the existing copied `Style` down the tree. `breaks_inside_words()` is the
  one predicate the breaker asks. `word-break: break-word` is parsed onto
  `overflow-wrap: break-word` as the spec says, and `normal` / unknown
  keywords restore the initial state.
- **The breaker only fires when a word still does not fit.** When the word
  fits, the run is emitted exactly as before, so pages without these
  declarations stay byte-identical. Once a word overflows a wrappable line,
  `fit_prefix` binary-searches the longest character prefix that fits
  (widths are monotone in prefix length) and the remainder continues on the
  next line. `word-break: break-all` and `overflow-wrap: anywhere` use the
  same path.
- **The cut is made at placement time, not by splitting atoms.** Splitting
  words into character atoms up front would multiply every run, underline
  rect and hover target; the greedy cut creates only the extra line pieces
  a real browser would show. Underlines / strike / overline are emitted per
  piece so decoration follows the visible line.
- **Progress is guaranteed.** If not even one glyph fits, a non-empty line
  wraps first; a character wider than the whole line is emitted anyway
  (the old overflow result). Emitted width always advances `x` or the loop
  breaks because the remainder is empty.
- **Shortcuts, documented here.** `anywhere` is treated as `break-word`
  (the min-content difference is ignored), `keep-all` behaves like `normal`
  (there is no CJK breaker), continuation lines reuse the current line's
  usable range instead of re-checking floats, `hyphens` / `line-clamp` are
  absent, and `text-overflow: ellipsis` is still not painted.
- **Wikipedia moved, and that is intended.** `/wiki/JavaScript` gains 7 run
  pieces and 121 px (24454 -> 24575) because its own rules
  (`.mw-body-content a.external.free { word-break: break-all }`,
  `h1 { word-break: break-word }`) now take effect; the other four
  references keep their exact geometry.

### Wave 48 findings

- **The log lives in the DOM host, behind a toggle.** `runtime::DomHost`
  pushes a `MutationRec` from `append_child`, `insert_before`, `remove_node`,
  `set_attribute`, `remove_attribute`, `set_node_data` and
  `set_text_content`, gated by a thread-local `TRACK_MUTATIONS` flag the script
  layer turns on at the first `observe` and off at the last `disconnect` /
  page replace. A page with no observer allocates nothing for mutations.
- **Each record snapshots its tree root.** Full-page frameworks create a
  detached subtree, mutate it, then insert it; filtering delivery against the
  live tree at turn end would report those pre-insert attribute changes to a
  document observer (and report them with the wrong `target` ancestry).
  `root_of` is recorded at mutation time, and `observed` requires both the
  target relationship and the mutation-time root, so off-screen work stays
  invisible until it is actually inserted.
- **Delivery is at the turn end, one batch per observer.** `with_ctx` drains
  the log after the turn body, matching each record against every live
  observer (`subtree` ancestry, attribute filter, old-value flags), then calls
  each callback with `(records, observer)`. A callback that mutates the DOM
  produces more records, so delivery repeats up to 8 rounds and logs
  `[js] mutation observer callback did not settle` instead of hanging.
- **`takeRecords` splits the shared log.** It consumes only the records
  matching that observer's own registration and puts the remainder back, so
  two observers on different targets do not steal each other's records.
  `disconnect` clears the target and drops the observer's strong reference.
- **`observe` implies its category.** As in the spec, `attributeOldValue` or
  `attributeFilter` implies `attributes: true`, and `characterDataOldValue`
  implies `characterData: true`; `observe` with no enabled category throws.
- **Shortcuts, documented here.** One observed target per observer (a second
  `observe` replaces the first), callbacks at turn end instead of true
  microtask checkpoints, no weak references, records are plain arrays, and
  `previousSibling` / `nextSibling` are always `null`. The log only sees
  script-visible mutations that flow through `DomHost`, not parser or layout
  internals.
- **No geometry change, as intended.** All five reference pages kept their
  exact height and run count; RAM moved only within run-to-run noise.

### Wave 47 findings

- **Constructed events are plain objects with hidden flags.** `Event` and
  `CustomEvent` instances get their prototype from the registered
  constructor, carry own `__pd` / `__sp` / `__sip` booleans, and expose
  `type`, `bubbles`, `cancelable`, `defaultPrevented`, `target`,
  `currentTarget`, `eventPhase` and `isTrusted`. `CustomEvent.prototype`
  chains to `Event.prototype`, so one `instanceof Event` check covers both;
  calling either constructor without `new` throws.
- **Dispatch is synchronous and owns no state.** The single `dispatchEvent`
  native walks the target's parent chain, then the shared
  window/document bucket, snapshotting the `(node, type)` listener list at
  each stop. Listener exceptions are logged and the walk continues.
  `stopPropagation` ends the walk; `stopImmediatePropagation` also stops the
  current stop. `target` is restored to `null` `currentTarget` and phase 0
  after the walk; the return value is `!defaultPrevented`.
- **`preventDefault` is cancelable-aware.** On script events it only sets
  `defaultPrevented` when `cancelable` is true. Shell event objects carry no
  `__pd`, so they keep the old host side channel and the link-navigation
  veto is unchanged.
- **Shortcuts, documented here.** Shell-dispatched events still lack both
  stop methods (each listener already receives a fresh wrapper, so a flag
  could not travel between them); there is no capture phase, no re-dispatch
  guard, and `document` and `window` remain one listener bucket.
  `createEvent` / `initEvent` / `initCustomEvent` are absent.
- **No geometry change, as intended.** All five reference pages kept their
  exact height and run count; RAM moved only within run-to-run noise.

### Wave 46 findings

- **DataView is a window, not a copy.** The instance stores the backing
  `ArrayBuffer`, its byte offset, and its byte length in three hidden own
  properties; reads and writes go straight through to the shared bytes, so
  a `Uint8Array` over the same buffer sees them immediately. The default
  endianness is big-endian and the optional `littleEndian` argument is
  honoured per call (typed arrays are little-endian by comparison).
- **Bounds are checked per access.** An offset outside the view, a length
  that overruns the buffer, or a non-ArrayBuffer first argument throws
  `RangeError` / `TypeError` instead of silently reading elsewhere. The
  BigInt setters require a BigInt and throw `TypeError` for numbers.
- **AbortSignal listeners fire synchronously from `abort()`.** Listener
  exceptions are logged and do not escape, matching event dispatch. The
  controller only carries `signal`; `abort()` is on the shared prototype.
  Duplicate `(type, callback)` registrations are ignored and
  `removeEventListener` removes them by identity.
- **Abort shortcuts, documented here.** An already-aborted signal calls a
  newly added listener synchronously rather than from a queued microtask;
  `AbortSignal.timeout` / `any` and signal `dispatchEvent` are absent;
  `fetch` still ignores a signal; the default reason is an `Error`-kind
  object named `AbortError` (no `DOMException` type), so `reason.name`
  works while `reason instanceof DOMException` cannot.
- **No geometry change, as intended.** All five reference pages kept their
  exact height and run count.

### Wave 45 findings

- **`queueMicrotask` rides the existing microtask queue.** Added a third
  `Job` variant, `Job::Call(Val)`, next to promise reactions and dynamic
  imports. `exec`'s turn-end drain runs it FIFO, so
  `queueMicrotask(a); Promise.resolve().then(b); queueMicrotask(c)` fires
  a, b, c in that order. A non-callable argument throws
  `TypeError: queueMicrotask callback is not a function`; the existing
  10,000-job drain cap still bounds runaway loops.
- **The new collections are snapshots, not live.** `document.forms`,
  `getElementsByName`, `Node.childNodes` and `Element.children` walk the
  arena and return plain arrays on each read. Writing to `forms` or using
  named access (`document.forms.login`) is not implemented; callers can
  index or iterate as with the existing `getElementsBy*` results.
- **Character-data wrappers are real nodes.** `createTextNode` /
  `createComment` append orphan arena nodes (`nodeType` 3 / 8, live `data`,
  `textContent`, parent/sibling navigation) that `appendChild` /
  `insertBefore` can attach. Element wrappers still snapshot
  `textContent` at wrap time, so a wrapper obtained before an append does
  not see the new child; a fresh `getElementById` read does. This is the
  long-standing snapshot shortcut, not a new regression.
- **`Element.children` filters by node kind.** It keeps children whose
  `nodeType` is 1, so text and comment nodes are excluded while
  `childNodes` keeps all four kinds.
- **No geometry change, as intended.** All five reference pages kept their
  exact height and run count, which also confirms the new host methods do
  not disturb layout.

### Wave 44 findings

- **Every property insert copied its key.** `Obj.props` is a
  `Vec<(Rc<str>, Val)>`, and the insert path did `Rc::from(key)` on every
  miss: one allocation plus one copy per property slot. A React-style
  render writes the same handful of names (`type`, `props`, `children`,
  `key`, ...) into thousands of objects, so the heap held thousands of
  identical strings.
- **Interning.** A thread-local `KEY_INTERN` maps each distinct name to one
  shared `Rc<str>` (`intern_key`), capped at `KEY_INTERN_MAX = 4096`;
  `JsEngine::new` clears it so a navigation cannot pin the previous page's
  names. The three paths that create property keys use it: `obj_set_own`,
  `set_member`'s function-property insert, and `Object.defineProperty`'s
  function-property insert.
- **Measured.** Deterministic workload (150k objects x 8 properties, same
  process, working-set delta): **108.0 MB without interning, 71.7 MB with
  it - 36.3 MB (~34%) of that workload's heap**. Live RSS is too noisy
  (+/-6 MB) to read the same number off a reference page, so the synthetic
  number is the honest measure.
- **Live pages unchanged.** nextjs.org 7578px / 569 runs, first layout
  ~152 MB, 0 script errors; MDN 2922px / 641 runs / 87.3 MB; Wikipedia
  24454px / 10943 runs / 101.1 MB; Hacker News 1695px / 728 runs / 78.1 MB;
  rust-lang 5800px / 483 runs / 84.7 MB; all error-free.
- **Limits.** Past 4096 distinct names per thread a new key is allocated
  normally (correct, just unshared); the table is only cleared when a new
  engine is built, so a very long-lived page can hold up to the cap. Keys
  stored outside these three paths (module export maps, literal string
  values) are still allocated per site.

### Wave 43 findings

- **`Object.getPrototypeOf` read only the explicit slot.** Prototypes are
  attached lazily on first member access (`get_member`), so a just-created
  array, promise, iterator or error had `proto: None` and `getPrototypeOf`
  reported `null`. core-js's `defineIterator` helper evaluates
  `Qr(y.call(new t)).next` on a fresh `Set` iterator; `Qr` saw `null` and
  threw `Cannot read property 'next' of object`. `JsEngine::proto_of` now
  answers the effective link without mutating the object: the explicit
  `proto` when set, else Array → `Array.prototype`, Promise/TypedArray/Error
  → their shared prototype, module namespaces → null, everything else →
  `Object.prototype` (with a self-prototype guard so `Object.prototype`
  itself reports null).
- **Every chain walk now hops lazily.** `chain_get`, the setter's accessor
  lookup, `instanceof` (four branches), the `in` operator and
  `Object.prototype.isPrototypeOf` used to follow only explicit `proto`
  links, so a user prototype object created by `function A(){}` never reached
  `Object.prototype`. They now use `proto_of` per hop: `new A() instanceof
  Object`, `'x' in a`, `new A().hasOwnProperty(...)` and
  `hasOwnProperty` after `new` all answer correctly, and
  `Object.prototype.isPrototypeOf(moduleNamespace)` stays false.
- **Live effect.** nextjs.org drops its final script error (1 → 0). Geometry
  is unchanged at 7578px / 569 runs; first paint is ~155 MB (was 148.7 MB)
  and, now that the app fully boots, its own router prefetches drive a
  handful of re-renders that settle around 162 MB without further growth.
  The rendered page matches the reference.
- **Other pages are byte-stable.** Wikipedia 24454px / 10943 runs / 102.1 MB,
  MDN 2922px / 641 runs / 87.4 MB, Hacker News 1695px / 728 runs / 78.1 MB,
  rust-lang 5800px / 483 runs / 84.8 MB; zero script errors on all four.
- **Limits.** Built-in iterators have no own iterator-prototype object, so
  `Object.getPrototypeOf(new Set().values())` is `Object.prototype` (the
  identity core-js's check short-circuits on) rather than an
  `Iterator.prototype` chain. `Object.getPrototypeOf(primitive)` still returns
  `null` instead of the wrapper prototype, and `Object.create(null)` remains
  indistinguishable from "not attached yet", so it also reports
  `Object.prototype`.

### Wave 42 findings

- **Comments were thrown away.** `tokenize` skipped `<!-- … -->` entirely.
  React's Flight bootstrap depends on them: the boundary marker before
  `<template id="B:n">` is a comment whose `data` flips `$?`/`$!`/`$~`/`$`,
  and `$RV` walks and rewrites those markers. Comments are now
  `NodeKind::Comment(String)` nodes, skipped by layout/text extraction and
  excluded from element queries (`Dom::is_element`), but visible to scripts.
- **Wrappers had no tree navigation.** Elements exposed only `parentElement`
  and `appendChild`. They now also expose `nodeType`, `parentNode`,
  `previousSibling`/`nextSibling`, `firstChild`, `removeChild` and
  `insertBefore`; text and comment nodes get a small `CharacterData` wrapper
  whose `data`/`textContent` read and write the arena string. `insertBefore`
  detaches the moved node from its old parent first, which is what `$RV`
  relies on when it hoists streamed children out of a hidden `<div>`.
- **`dataset` is a cached snapshot.** `el.dataset` builds a camelCase view of
  `data-*` attributes and caches it on the wrapper, so `$RX`'s
  `a.dataset.dgst = …` writes read back on the same wrapper. The writes do
  not become attributes and a fresh wrapper rebuilds from the arena — an
  honest shortcut, enough for error markers.
- **Snapshot format bumped to v2.** Comment nodes are serialized, so the DOM
  cache needed a new version; old snapshots miss once and refetch. That is why
  MDN's numbers moved (2675 -> 2922 px): the previous snapshot was stale and
  the fresh page carries more content (and more comments). The rendered top
  matches the reference and the page stays error-free.
- **Live effect.** nextjs.org drops 6 -> 1 script error (only the core-js
  iterator-helper `.next` on a null receiver remains). With `$RV` finally
  running, the streamed footer boundary is swapped into place: layout goes
  7032px / 502 runs -> 7578px / 569 runs and RAM 145.2 -> 148.7 MB. The five
  other pages are visually unchanged; Wikipedia and rust-lang are byte-stable
  in geometry.
- **Limits.** Engine B (compile-gated) still wraps only the wave-17 element
  surface. Comments are not exposed as `childNodes`/`children` collections,
  `document.createComment` is absent, and `dataset` writes never reach the
  `data-*` attributes.

### Wave 41 findings

- **`performance` was simply missing.** Nothing in `builtins`/`script` defined
  it, so `performance.now()` threw `Cannot read property 'now' of undefined`
  inside nextjs's `requestAnimationFrame(function(){$RT=performance.now()})`
  bootstrap (surfacing once per frame as `[js] rAF error`). A process-wide
  `OnceLock<Instant>` epoch now backs `performance.now()`; `timeOrigin` is the
  wall-clock instant that zero maps to. `mark`/`measure`/`getEntries` are
  no-ops (`getEntries*` returns an empty array) because pages only probe them
  for analytics.
- **rAF callbacks had no timestamp.** `run_due_frames` called each callback
  with zero arguments (a documented shortcut from the DOM wave). It now passes
  `perf_now_ms()`, so `(t) => …` animation code sees a finite number on the
  same clock as `performance.now()`.
- **Live effect.** nextjs.org's rAF error disappears: 7 -> 6 script errors,
  with layout and RAM essentially unchanged (7032 px / 502 runs / 145.3 MB vs
  145.8 MB). The four other reference pages are byte-stable in geometry and
  error-free. The five remaining `Cannot set property 'data' of undefined`
  errors are a separate gap: their scripts read `previousSibling` of a
  `<template id="B:n">` marker and expect a comment node with writable `data`,
  but this DOM exposes no sibling navigation and drops comments entirely.
- **Limits.** The epoch is process-wide rather than per-page/navigation, so
  `performance.now()` keeps counting across tab loads; `mark`/`measure` do not
  record entries; `performance.timeOrigin` is a one-shot number, not a live
  property.

### Wave 40 findings

- **Regex flags were under-lexed.** `read_regex` consumed only `g i m s y`, so
  `/[A-Z]/u` lexed as a regex literal followed by the identifier `u`. Both
  failures on nextjs.org were this shape: `/[A-Z]/u.test(t)` (expected `)`)
  and `/^matrix3d\(…\)$/u` inside `String.match` (expected `,`). The lexer now
  also accepts `d`, `u` and `v`; the regex engine accepts them as no-ops
  because `u`/`v` only change escape/class grammar for the subset it supports
  (`\p{…}` still fails cleanly) and `d` only adds match indices.
- **Non-ASCII identifiers were rejected.** The lexer treated every byte >= 0x80
  as an unexpected character. It now accepts them in identifier position and
  consumes the whole UTF-8 sequence, which slices on a char boundary because
  ASCII bytes never appear inside a multibyte character. This is deliberately
  looser than Unicode ID_Start/ID_Continue (emoji pass too) — documented as a
  shortcut; it unblocked lodash's `deburr` table (`{Ä:"A", é:"e", …}`), which
  is emitted as real object keys in the vendored bundle.
- **Computed keys in destructuring were a parse error.** `let {[e]: r, …rest}`
  hit `prop_name()`, which only accepts identifiers/strings/numbers. Patterns
  now carry `ObjPatPart::ComputedProp { key: Expr, part }`; the key expression
  is evaluated at bind time against the current scope, stringified, and also
  recorded for `...rest` exclusion. Binding-name collection, `arguments`/`yield`
  usage scans and the leaf-name walk all recurse into the pattern part and
  visit the key expression where appropriate.
- **Live effect.** nextjs.org's four syntax errors disappear (11 -> 7 total
  script errors); layout stays 7032 px / 502 runs because the newly-running
  chunks are framework code that does not paint yet, and RAM rises 140.8 ->
  145.8 MB as that code now executes. The other four reference pages are
  byte-stable in geometry and error-free. Remaining nextjs errors are runtime
  gaps: `.next` called on a non-iterator, inline scripts writing `.data` on an
  undefined value, and a `performance.now` call on an undefined global.
- **Limits.** `u` semantics are accepted but not enforced (surrogate pairs,
  `\p{…}`, strict escape rules); a repeated `u`/`v`/`d` flag is not rejected;
  and computed keys in `=`-form object-assignment patterns still go through the
  old expression-to-pattern converter.

### Wave 39 findings

- **Grid `justify-content` was a documented no-op.** Column widths came from
  `resolve_tracks`, which unconditionally stretched `auto` tracks into the
  leftover space, and every item was packed at the content-box start. Any
  author value (`center`, `end`, the three `space-*` forms) was parsed and then
  ignored by the grid path.
- **Sizing vs. distribution are now separate.** `resolve_tracks` takes a
  `stretch_auto` flag that is true only for `justify-content: normal` (the
  initial value; `stretch` is an alias). "Maximize tracks" still grows
  content-sized tracks to their growth limit in every case — that step is
  intrinsic track sizing, not alignment. Whatever is left afterwards is handed
  to `grid_content_offsets`, which returns a leading offset and an extra gap:
  `center`/`end` shift the whole line, `space-between` splits the slack over
  the `n-1` inter-track gaps, `space-around` puts half a share at each edge,
  `space-evenly` splits it into `n+1` equal spaces. A spanning item's width
  grows with the extra gap it covers, matching the grid-area rule.
- **Flex had the same conflation.** `space-around`/`space-evenly` were parsed
  into the `SpaceBetween` variant and therefore rendered as `space-between`
  (edge items flush to both sides). They now have their own variants and
  positions; with a single item both centre it, which is what the spec's
  distribution resolves to.
- **Live effect: none on the four reference pages.** Their grids either use a
  `1fr` track (which already absorbs the slack, leaving nothing to
  distribute) or no explicit `justify-content`; every geometry number is
  unchanged (MDN, nextjs, Wikipedia, HN), and no new script errors appear. The
  change is spec fidelity for pages that do use the property, proven by unit
  tests for each value and by the auto-stretch flag test.
- **Remaining gap.** `align-content` (the block-axis twin) is still unparsed,
  so a grid taller than its rows packs them at the top; `justify-items` /
  `place-content` shorthands are likewise absent.

### Wave 38 findings

- **`getElementsByClassName` was missing entirely.** HN's cached `hn.js` defines
  `byClass(el, cl) { return el ? el.getElementsByClassName(cl) : [] }` and calls
  it on `document` and on elements; the `undefined is not a function` at
  `hn.js` line 2 aborted the rest of the script.
- **Semantics.** The name list is the argument's string form split on ASCII
  whitespace, and an element matches only when it carries **every** name
  (class names compare case-sensitively). An empty list matches nothing, per
  spec. Results are snapshot arrays in document order — the same policy as the
  existing `getElementsByTagName` — not live HTMLCollections. On an element the
  search walks descendants only and excludes the element itself.
- **Element-level `getElementsByTagName`** landed in the same pass, since HN's
  `byTag` helper uses both forms; it shares `subtree_elements_dom` with the
  class search, so one subtree walk backs both.
- **Cost/limits.** No new per-node state: a query allocates one `Vec<u32>` of
  matches (a snapshot would anyhow) and one wrapper per returned node. On HN
  the only observable change is the error line disappearing — 734 runs, 1695px
  and 78.1 MB are identical, so the script had no layout work left to do once
  it stopped throwing. Wikipedia and MDN are unchanged with zero script
  errors. Still missing around this surface: `document.getElementsByName`,
  `document.forms`, and live-collection mutation semantics (a snapshot does not
  see nodes added after the call).

### Wave 37 findings

- **`new Function` was returning a plain object, not a function.** The
  constructor's stub handed back a native that ignored its body and returned
  the global object; that only worked for `Function("return this")()` — called
  *without* `new`. The `new` operator keeps a constructor's result only when it
  is an object, and a `Val::Native` was not in that set, so `new Function(...)`
  evaluated to the discarded fresh object. Calling that (`new Function(...)()`)
  threw `TypeError: object is not a function`.
- **That broke MediaWiki's module loader.** ResourceLoader evaluates every
  module's `skip` condition as a source string:
  `var skip = (new Function(registry[module].skip)())`. Each evaluation threw,
  `sortDependencies` aborted, and the module was dropped as
  `Skipped unavailable module …` plus `Exception in resolve` — 20 times on
  `/wiki/JavaScript`.
- **The constructor now compiles its body.** Arguments are stringified (last is
  the body, the rest are the parameter list), wrapped as
  `function anonymous(params\n) { body }`, parsed by the common parser and
  closed over the **global** environment (not the caller's). Destructuring,
  default and rest parameters therefore work; a syntax error becomes a
  throwable `SyntaxError`; the function's name is `anonymous`, as specified.
  `new` now honours any object-like constructor result (function values
  included), which is what lets `new Function(...)` return the compiled
  function.
- **Live effect.** Wikipedia's loader resolves cleanly: `Exception in resolve`
  0, `Skipped unavailable module` 0, zero script errors, identical geometry
  (24454 px / 10943 runs / 102.2 MB) — with the worker finishing in 12.4 s
  against 18.2 s in the wave-36 run. MDN homepage (2675 px / 615 runs /
  99.7 MB), nextjs.org (7032 / 502 / 140.5 MB, same 11 pre-existing errors) and
  rust-lang.org (483 runs) are unchanged; Hacker News keeps its one
  `getElementsByClassName` gap, which is an element-API issue, not this path.
- **Limits.** A generated function's syntax-error position is reported inside
  the wrapper, not body-relative; `eval` is still absent; and the compiled body
  runs under the interpreter's existing strictness rules (a leading
  `"use strict"` is parsed in the body but strict-only semantics are not
  separately enforced).

### Wave 36 findings

- **The failing read was in the shared runtime, not the entry chunk.** MDN's
  `runtime.7038046b4a18dfad.js` defines `m = s => { … for (i = 0; i < r.length; i++) … }`
  where `r = s.__rspack_esm_ids`. The entry chunk imports the runtime, then at
  the tail of its own body calls `e.C(r)` — passing **its own namespace** as
  `r` (rspack/webpack emit exactly that self-import shape). Imports resolve
  before the body executes, so `r` was a namespace snapshot taken when the
  entry had exported nothing yet: `r.__rspack_esm_ids` was `undefined` and
  `.length` threw.
- **Namespaces are now live views, not copies.** `import * as ns` (and
  `import()`'s value) builds an `ObjKind::ModuleNs(Exports)` object whose own
  storage *is* the module's export map. `chain_get`, `obj_get_own`, `obj_has`,
  `for..in`, `Object.keys/values/entries`, `JSON.stringify`,
  `Object.prototype.toString` and value printing all consult that map at access
  time, so a self-importing entry chunk sees the exports its body records
  later. Namespaces get no `Object.prototype` link (`'toString' in ns` is
  false), matching the spec's null-prototype namespace exotic.
- **The error log also named the wrong file.** A throw inside the runtime was
  attributed to the entry chunk (the module being evaluated) while the
  `(line, col)` resolved against the runtime's source — which is why the
  reported column landed inside a CSS string. When the throwing function's
  source differs from the evaluating module, the log now says
  `entry.js (throw in runtime.js): …`; the location and the file agree.
- **Tests.** `self_import_namespace_is_live_during_the_body` reproduces the
  rspack shape (self-import, read an export recorded after the import line,
  assert no `Object.prototype` leak) and
  `error_origin_names_the_defining_module_when_it_differs` pins the two-file
  log format. Suite 748 default / 752 `engine-b + woff2`.
- **Limits.** Namespace reads are live but writes still land in `props` (a
  namespace is read-only in the spec; nothing in the wild assigns to one).
  Module bindings imported *by name* remain copies taken at link time — only
  the namespace object tracks later exports — so a cycle that reassigns a
  named `export let` is still not observed. Dynamic `import()` in a cycle keeps
  returning the still-empty map rather than waiting for evaluation.

### Wave 35 findings

- **The gap was the cascade, not the rasterizer.** Inline `<svg>` is RAW_TEXT
  in the DOM, so `svg.rs` rasterizes it with no access to the page's styles:
  an SVG child only painted if it carried its own `fill`/`font-size`. Wave 21
  closed the easy half by baking *single-class* rules into inline `style`;
  MDN's homepage mandala defeated that shortcut — its colours come from
  `.mandala svg>text { fill: var(--color-border-primary) }` (HTML ancestor,
  descendant + child combinators) and its ring sizes from
  `.mandala textPath[href="#circle1"] { font-size: 1.5rem }` (attribute
  selector), so the SVG root's `fill="none"` won and nothing painted.
- **Mini-cascade over the SVG's ancestor chain.** `Sheet::svg_candidates`
  prefilters rules once per SVG (subject tag must occur in the markup;
  pseudo-classes, `:not()`, pseudo-elements and sibling combinators are out),
  and `Sheet::svg_decls` matches one element against a `SvgElem` chain:
  tag / `#id` / `.class` / `[attr]` compounds with descendant and child
  combinators, last-in-cascade wins per property, `var()` resolved against
  the page palette and `light-dark()` against the page polarity — the same
  values the text cascade would produce. `layout::build` hands in the `<svg>`
  element's real HTML ancestors (walked via `Node::parent`), and the markup
  walker pushes/pops the SVG's own open elements, so `.mandala svg>text`
  matches the wrapping `<div class="mandala">` and not some unrelated rule.
  `font-size` joined the merged property list, and `svg.rs` now resolves
  `rem` (16 px root) and `em` (relative to the current size).
- **Live effect.** The MDN homepage hero's right side went from empty to the
  five rings of slashes (colour `--color-border-primary`, sizes 1.5/1.3/1.2/
  1.1/1 rem at the viewBox scale). Page height (2675 px), runs (615) and RAM
  (~100 MB) are unchanged — SVG text is blitted by the shell, not counted in
  the layout run list. Wikipedia `/wiki/JavaScript` is unchanged
  (24454 px / 10943 runs). A local probe with the real mandala markup plus
  the page's own rules paints the same five rings; the same page with MDN's
  3.8 MB stylesheet loads in 308 ms, so the candidate prefilter keeps the
  per-tag scan cheap (rules whose subject tag isn't in the SVG are dropped
  once, before the walk).
- **Limits.** Sibling combinators (`+`, `~`) and pseudo-classes never match
  inside the injected markup; `<style>` blocks authored *inside* an SVG are
  still not read; custom properties resolve against `:root`'s palette, not a
  `style="--x:…"` on an SVG node (those nodes aren't in the DOM and so have
  no `VarScope`); and attribute names compare case-insensitively while values
  follow the selector's operator (`=` folds ASCII case, like the DOM path).

### Wave 34 findings

- **A row-spanning item sizes its rows, not just its own box.** `grid()`
  painted a spanning item from its start row and let the rows it covered size
  themselves from single-row content only, so a tall span (MDN's hero
  `div.homepage-header__mandala`, 351x420, `grid-area: mandala`, spanning the
  copy/search rows) overhung the grid and everything after it moved up under
  the overflowing box. The fix records the item's margin-box height
  (`span_h`) while it is laid out in its **start** row — its height does not
  depend on where it sits vertically, so there is no second layout pass — and
  when the loop reaches the row where the span ends, compares that height with
  the rows already committed (`row_hs[r0..r] + row_h + gaps`) and adds the
  shortfall to the **last covered row** (`shortcut:` CSS distributes the
  extra equally across the covered rows; growing the already-painted ones
  would need a second pass, so the last row takes it all — the span's bottom
  edge is exact, intermediate row boundaries can differ). Spans ending on the
  same row are handled narrowest-first, so a 3-row span sees the row heights
  a 2-row span just produced. `grid_row_span_stretches_its_last_row` pins all
  of it: 2 rows (30px + 10px gap + 40px) plus a 120px span → row 1 still
  starts at 40px, the grid is 120px tall, and the span's bottom edge equals
  the grid's.
- **Live effect.** MDN's hero grows 288 → 420px (the mandala's height, which
  used to overflow into the "Featured articles" heading), "Featured articles"
  moves from y=368 to y=500 and the page from 2542 to 2675px — 615 runs and
  RAM unchanged (~99 MB). Wikipedia `/wiki/JavaScript` is byte-identical at
  24454px / 10943 runs: none of its grids has a multi-row item.
- **Relayout-storm check (not reproduced).** One wave-34 run showed 8 layouts /
  25 job generations with `skipped N superseded relayout(s)` in bursts. It
  did not reproduce: four later runs (three of the wave-34 binary, one of the
  wave-33 build), launched the same way with the pointer untouched, each did
  exactly one layout (`applied #1`) with no superseded relayouts. An
  instrumented debug build logged zero `[hover]` dispatches in the clean
  runs. The hover branch is the only relayout source that can fire repeatedly
  without a load or a resize (it is gated on pointer motion), so the bursts
  are consistent with the pointer being moved across the page while the 3s
  layouts were in flight — each new hit node queues a job and the worker
  coalesces the queue. Load time is not a wave-34 cost either: MDN worker
  finishes were 3.0–4.1s for both builds that evening (an earlier 1.8s
  sample was a warm outlier), and `span_h` adds one f32 per grid item.

### Wave 33 findings

- **A spanning item contributes its shortfall, not its width.** `grid()`
  measured every item and `max()`ed its min/max-content into every column it
  covered, so an item spanning `1fr max-content` sized BOTH columns from its
  own one-line width: MDN's news summary (615px) turned the `max-content` date
  column into 615px and left the `1fr` column 0px, painting the date on top of
  the source link. The new `span_contribution` (called per column measurement,
  single-column items first, then spans narrowest-first) computes the shortfall
  over the covered columns plus the gutters between them and hands it out by
  the CSS rule: a span crossing a flexible track feeds only those flexible
  tracks (the intrinsic ones are subtracted from the contribution first, since
  CSS sizes flexible spans after the intrinsic tracks); otherwise the covered
  intrinsic tracks share it equally; if none is intrinsic, every covered
  column does. Verified by `span_contribution_splits_the_shortfall` and the
  end-to-end `grid_span_does_not_widen_intrinsic_columns` (the card shape with
  a 100-char summary word: date column 16px, `1fr` 584px, summary spans 600px).
- **Intrinsic widths were measuring the HTML source, not the text.** The second
  half of the same card bug: even with spans fixed, the `max-content` column
  stayed 251px wide for a 58px date. A text leaf keeps the newlines and
  indentation written between tags, and `natural_w_inner` measured that raw
  string (`"\n                5 months ago\n              "`). `collapsed_w`
  now folds collapsible whitespace exactly like line layout (a run = one space,
  edges disappear, whitespace-only leaves stay one separator so inline
  siblings keep their space - `Ws::preserve()` leaves are measured raw).
  `intrinsic_width_collapses_formatting_whitespace` guards it. Live effect:
  the date lands at x=398 instead of 121.
- **Cost/limits.** No extra allocation in the layout hot path (`collapsed_w`
  sums word and single-space measurements), no new wire fields, no new state.
  The MDN dump is byte-identical except the news rows (2542px, 615 runs); RAM
  is unchanged within run noise (89.1 vs 100.9 MB across builds is frame/GPU
  noise, the layout delta itself allocates nothing). Wikipedia's page grows
  89px because a couple of shrink-to-fit boxes are now their true content
  width rather than content + source indentation; that is the correct
  direction, and the capture's section heights stay within one wrapped line
  per box. Known remaining gap in this area: shares are equal rather than
  water-filled up to each track's growth limit, a span over several flexible
  tracks splits by flex factor, and grid `justify-content` is still a no-op
  (the MDN card's `space-between` lands right only because its `1fr` column
  absorbs the slack).

### Wave 32 findings

- **A click on `<label>` text now activates its control.** The worker already
  walks every hit-test element for the inspector; it now also records
  `ElementInfo.label_for` there, resolved once per `<label>` per page wire
  (`label_control`: the `for=` target when that id names a labelable element,
  else the label's first labelable descendant in tree order). The shell's
  click path checks it after the link test and hands the control's own
  `FormBox` to `on_form_click`, so a label behaves exactly like the control:
  checkbox/radio toggle + listeners, text-field focus with its value seeded,
  submit. The mapping is per *element inside* the label, so nested inline
  children (`<label>Pick <span>one</span>...`) activate too.
- **Label state travels with the page, not the DOM.** The shell has no DOM, so
  the resolution rides the existing `ElementInfo` channel (8 bytes per hit-test
  element) instead of a new wire field; no extra thread hop and no synthetic
  input in the test path.
- **Implicit submission names the form's default button.** `implicit_submitter`
  maps the activated node to a submitter: a submit button names itself, a
  text control (Enter) names the form's first enabled submit button - the
  browser's "implicit submission clicks the default button" rule - and
  `form.submit()` (the form is activated) names nobody. This replaced the
  previous "only a clicked submit button is a pair" rule, which dropped a
  server-visible pair on every Enter-key submission.
- Honest gaps left in this area: a disabled first submit button is skipped
  (correct) but a *label* pointing at a `<select>` still only blurs (no popup
  exists), `for=` matching is by id only, and clicking a label whose control
  is not laid out (display:none) does nothing.

### Wave 31 findings

- **A radio click now moves the whole group.** `toggle_control` (split out of
  the old `apply_click_toggle` so it can be tested on a bare DOM) clears
  `checked` from every same-`name` radio that shares the clicked radio's form
  owner - or the whole document when the group has no form - and then sets
  the clicked one. A click on an already-checked radio keeps it checked
  (browsers never uncheck a radio by clicking it); checkboxes still toggle.
  Same-name radios in a *different* form keep their state, which is the part
  a naive "clear every radio with this name" would get wrong.
- **Disabled controls are no longer submitted.** `control_disabled` checks the
  control's own `disabled` attribute and any disabled `<fieldset>` ancestor;
  the walk skips such subtrees, so a disabled fieldset costs one ancestor
  check per node and nothing else. The spec's first-`<legend>` escape hatch is
  not modelled (a control inside a disabled fieldset's first legend also
  stays out).
- **`<select>` submits its selected option.** Previously the whole control was
  inert, so any GET/POST form with a language/region/filter select went out
  without it. The submitted value is the option's `value` attribute, else its
  label text with whitespace collapsed; a select with no marked option uses
  the first enabled one (browsers' implicit selection), disabled options are
  skipped for that implicit pick, and a disabled select contributes nothing.
  `multiple` still submits one value: no popup exists to pick several.
- **The activated submit button submits its own pair.** `collect_form_pairs`
  grew a `submitter: Option<u32>` argument; `build_submission` passes the node
  the shell activated, so a clicked `<button type=submit>` / `<input
  type=submit>` contributes `name=value` while the other buttons in the form
  do not. A text-field activation (Enter) names no submitter, so it still adds
  no pair - the implicit-submission rule that would pick the form's default
  button is not modelled (one line to add later if a site depends on it).
- Live proof stayed input-free: a local page's inline script calls
  `form.submit()`, and the worker's `[js] form.submit() -> GET ...` line shows
  exactly the pair set above. The click side (radios, submitter) is covered by
  the in-crate tests, since this shell never synthesizes user input.

### Wave 30 findings

- **The tab list now lives on disk.** `%LOCALAPPDATA%\mazda-cache\session`
  (next to the DOM snapshots and the cookie jar) is `active=<index>` followed
  by one URL per line, verbatim - no escaping, so `?`, `&`, `=` and `#` in a
  URL cost nothing and a corrupt line is dropped instead of costing RAM. Caps:
  64 tabs (`SESSION_MAX_TABS`), 16 KB per URL (`SESSION_MAX_URL`). A file with
  no usable tab line reads as "no session" and the shell starts one blank tab,
  so a truncated file costs the list, never a crash.
- **Restore rides the existing snapshot path.** At startup with no URL on the
  command line the saved list becomes the tabs and the active tab dispatches as
  a restore (`LoadReq::Load { restore: true }`), which hits the wave-9 DOM
  snapshot and skips fetch+parse; the live probe shows the header
  `[dom] restored snapshot for https://en.wikipedia.org/wiki/JavaScript
  (12788 nodes, no fetch)`. The other tabs stay URLs only and load on their
  first activation, exactly like a hand-opened tab, so restoring 8 tabs costs
  8 strings, not 8 pages.
- **Writes are digest-gated.** `session_digest` (FNV-1a over the URL list plus
  the active index) is compared against the last write; an idle frame, a
  scroll, a hover relayout and a repaint all leave the file alone (verified:
  mtime unchanged across 14 s of idle). The write happens from `ui()` rather
  than `on_exit`, because the process is routinely killed (`Stop-Process`) and
  a crash must not lose the list - eframe's `on_exit(&glow::Context)` would.
- **Scroll offsets are not persisted.** Honest shortcut: a restored tab starts
  at the top. Persisting them means another per-tab field in the format and a
  scroll-into-place path after restore; tab switches inside one run still keep
  their offset in RAM.
- **Phase 3 is nearly closed by construction.** Tabs are reload-on-switch, so
  no page is resident to hibernate: the DOM snapshot on disk *is* the
  hibernated form and the pressure policy is "nothing to discard". What is
  still open on that bullet is forms (POST bodies, `input` state across a
  history move) rather than the store.

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
- **Two hot-path fixes from the audit.** `**` with a BigInt exponent answers
  bases `0`/`1`/`-1` directly and rejects exponents above 127, instead of
  multiplying up to `u32::MAX` times (`1n ** 4294967295n` used to spin for
  ~4 billion iterations — the JS step budget does not cover builtin loops).
  `new TypedArray(len)` reserves the zeroed bytes and wraps a view over them,
  instead of building a `len`-element `Vec<Val>` (~24 B/element) first; the
  `1<<24` element cap still applies. Guards:
  `bigint_pow_does_not_spin_on_huge_exponents`,
  `typed_array_length_ctor_zero_fills_without_a_value_vector`.
- **Fixed in wave 33**: MDN's "Latest news" date used to paint on top of the
  source link - a grid track-sizing bug, not a missing feature. A multi-column
  span contributed its full width to every covered column, so the summary
  `<p>`'s one-line max-content width inflated the `max-content` date column and
  collapsed the `1fr` column. `span_contribution` now adds only the shortfall
  over the covered columns, narrowest span first, and a span that crosses a
  flexible track feeds only that track (CSS sizes such items after the
  intrinsic ones). A second, smaller inflater shared the blame: intrinsic text
  measurement folded the page's source indentation into the leaf width (the
  date leaf `"\n   5 months ago\n  "` measured 251 px for a 58 px string);
  `collapsed_w` folds it like line layout does. On the live homepage the date
  now starts at x=398 (card ends at 463) instead of x=121 over the source.
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
- **The UA's engine tokens are assembled from fragments** (`concat!` in
  `net.rs`, and the first `Sec-CH-UA` brand in `script.rs`) so the repository
  never spells another engine's product name, while the bytes on the wire stay
  the exact sequence servers sniff. A unit test pins the assembled shape and the
  header list (no duplicates).
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
  `feTile`; `clip-rule` is always nonzero; `marker` remains skipped. `<text>`
  is still not rasterized by this pass: `text_runs()` hands the runs to the
  shell, which owns the font metrics (wave 23).
- **Verification note**: the full suite ran in a clean worktree at `HEAD`
  with only `src/svg.rs` + `src/html.rs` overlaid — concurrent sessions had
  `src/js/*` mid-edit in the shared tree, so those changes were deliberately
  not pulled in. With every wave merged the suite is **736** default and
  **740** with `engine-b + woff2`, all pass.

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
- Tests: default **703** on the wave-25 base (**736** with waves 23-29
  applied), `engine-b + woff2` **707** on the base (**740** merged); five new guards
  (`min_height_floors_the_border_box_by_default`,
  `min_height_content_box_floors_the_content`,
  `declared_height_follows_box_sizing`, `aspect_ratio_follows_box_sizing`,
  `grid_percent_rows_resolve_against_the_content_height`), plus the mask paint
  snapshot updated to the new border-box `min-height:60px` geometry.
- Known gaps: declared `height` on a plain block is still ignored (only
  replaced/aspect-ratio/grid boxes consult it); `%` `min-height` still resolves
  against width, not the container height; `%` vertical padding uses the same
  width-basis shortcut as before.

### Wave 24 findings

- **A block/flex-column child contributes its MARGIN box to the parent's
  max-content width, not just its border box.** Negative margins therefore
  *shrink* the shrink-to-fit parent — Vector 2022's `.vector-main-menu-landmark`
  is exactly that: a 20px flex item whose only child is the 44px menu button
  with `margin: 0 -12px`, so it contributes 20, not 44. Ours reserved 44 and
  pushed the Wikipedia logo 24px to the right. Auto margins contribute nothing.
- **`content_min_w` (the min-content measure) now adds child margins too**, so
  the same rule holds on the automatic-minimum path: a flex item's floor is its
  min-content *margin* box. Both the max-content and min-content contributions
  agree, which is what a flex base and the `min-width: auto` floor both need.
- **Formatting whitespace between block-level children is not content.** Text
  runs keep the source's newlines/tabs (`"\n\t\t\t\t\n"` measured 43.7px), so a
  container whose children are all boxes was being widened by its own
  indentation — that whitespace alone was the landmark's 43.7px. Whitespace is
  now skipped in the max-content measure unless the container has real inline
  content, where a single space still separates inline boxes.
- **`calc(-12px)`** (a single negative argument) parses; the site's
  `.vector-button-flush-left` uses it for the flush-left margin.
- Verified against the reference at vw=1000 on `/wiki/Main_Page`: header start
  24..204, landmark 24..44 (20 wide), dropdown 12..56 (44 wide, `margin: 0 -12px`),
  logo box 64..204 — the wordmark moved 24px left, matching the reference.
  Note the page area itself is inset 12px in the window, so on-screen pixels are
  layout + 12 (this is what made the button *look* 12px off).
- **Known gaps**: nothing left in this chain; the intrinsic rules still model
  only horizontal margins (no `margin` collapsing, no vertical intrinsic
  contribution), and inline-block baselines are approximated elsewhere.

| 2026-11-03 | wave 23 (SVG text) | MDN homepage | 101.1 MB (615 runs; layout unchanged; the mandala still paints nothing - its `fill`/`font-size` come from page CSS applied *inside* the inline `<svg>`, which the image pipeline does not cascade) |
| 2026-11-03 | wave 23 (SVG text) | local page, inline `<svg>` (5 rings + anchors) | 76.9 MB (0 runs; 2 images drawn - five concentric slash rings plus start/middle/end `<text>` in three colours) |

### Wave 23 findings

- **The vector rasterizer cannot measure glyphs, so `<text>` is split across
  two layers.** `svg.rs` walks the document and emits `TextRun` records in
  *image* pixels (origin, size, colour, alpha, `text-anchor`, the `<textPath>`
  polyline, `textLength`, `dy`); the shell lays the runs out with the only
  font metrics in the process and blits them into the SVG's ARGB buffer.
  Content flattening is the parser's job (tspans flattened, entities decoded
  including `&nbsp;`, whitespace collapsed, NBSP kept).
- **Text paints after the shapes**, not in document order (shortcut): a run's
  glyphs are positioned before the SIMD-free blit and there is no z-slot for
  them inside the vector pass.
- **`textPath` needs the polyline in image space**, so `text_runs` re-derives
  the root viewBox matrix and harvests `<path id>` polylines (`harvest_paths`).
  Glyphs ride the path, stay upright (the spec's `rotate="0"` default), and an
  explicit `dy` moves them along the normal `(-ty, tx)` - which is why the rings
  sit *inside* their circles, as the reference does.
- **`textLength` is applied as one uniform advance scale** for the whole run;
  the reference distributes the extra as inter-glyph spacing. Identical total
  length, slightly different rhythm on very short runs.
- **`textPath`'s own `font-size`/`fill` style its glyphs** (MDN sizes each ring
  separately: 24 / 20.8 / 19.2 / 17.6 / 16 px).
- **Blending must not drop destination alpha.** The page buffer is opaque, but
  the SVG image composites over the page, so `blend_argb` keeps `A` while the
  page's `blend` still writes `00RRGGBB`.
- **Cost:** an SVG is now walked twice (`render` + `text_runs`); both passes are
  a tag scan over a few KB, so icons are unaffected - but it is a second parse,
  recorded here so a later profiler does not "find" it as a surprise.
- **Known gaps after wave 23**: `<tspan>`-level `dx`/`dy`/`style`/`fill` are
  flattened (one style per run); no `rotate`/`letter-spacing`/`word-spacing`;
  no `<text>` inside `<use>`; page CSS does not reach inside inline SVG, so
  MDN's mandala stays blank until that cascade exists (the SVG itself is
  verified by the inline-page row above); `dominant-baseline` and
  `alignment-baseline` are ignored.

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
  the position mapping, not the read, is suspect — **fixed in wave 36**: the
  read was the rspack runtime's `executeModules.length` over the empty
  namespace of a self-importing entry chunk, and namespaces are now live); SVG `<text>`/`textPath` are
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
  property 'length' of undefined` (index.js col 19277 — **fixed in wave 36**:
  the throw was `executeModules.length` in the rspack runtime, reading the
  empty namespace of a self-importing entry chunk); Wikipedia still throws
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
