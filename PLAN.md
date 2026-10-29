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
  Known gaps: no text/glyphs, no clip/mask/filter/pattern, no `use`/
  `image`, no CSS <style> blocks, gradients flat (not interpolated),
  no stroke-dash/miter/linecap variants
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
  string-level (nested CSS custom props inside the SVG ignored)
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
  Known gap: inline-block boxes stack as blocks instead of sitting mid-line; inline
  margins approximate to one space (not pixel-exact)
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
  still use generics until a Brotli+woff2 decoder chain lands; no bold/italic
  variants for installed aliases (regular file only); webfont face selection
  is a coarse (weight-range, italic) bucket — no synthesis
- [ ] **Phase 3** — tabs + **hibernation: restore from serialized DOM+layout on disk, not
  re-fetch** (Recommendations §2), cookies, forms, memory-pressure discarding policy
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

JavaScript, GPU compositor, process sandbox, full CSS, media playback, extensions, DevTools.

## Environment notes

- Toolchain: rustc 1.96.0, registry mirror rsproxy.cn (had corrupted sources once —
  fix = delete affected `registry\src` dirs and rebuild)
- `cargo build -j 1` used during flaky ICE episode; normal parallel builds since OK
