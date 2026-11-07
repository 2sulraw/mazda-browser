# Mazda Browser - Gap Analysis: HTML / CSS / JavaScript

**Date:** 2026-11-05
**Revision:** rewritten after a source review. Corrected the selector,
`sessionStorage`, and corner-radius claims; removed an unsourced parity estimate;
replaced raw spec-diff percentages with annotated inventory; and removed
third-party project names and vendor-prefixed spellings.
**Scope:** `src/html.rs`, `src/css.rs`, `src/layout.rs`, `src/js/*`, `src/script.rs`
**Method:** compare the engine's source-level surface with machine-readable spec
datasets, then check every contested item against the source.

> **How to read this.** A property or interface counted as implemented can still
> have behavioural gaps (see `PLAN.md` "Known gaps"). A property counted as
> missing is absent from the parser's property table, but may still work through
> a shorthand. More importantly, the percentages below are *inventory against a
> spec dataset*, not a quality score and not a roadmap. The project's success
> metric is steady-state RAM plus seven reference captures; Â§8 is the part that
> decides what to build next.

---

## 1. Sources

### Normative documents

| Area | Document | URL |
|---|---|---|
| HTML | WHATWG HTML Living Standard | https://html.spec.whatwg.org/ |
| HTML | Element index | https://html.spec.whatwg.org/multipage/indices.html#elements-3 |
| DOM | WHATWG DOM Standard | https://dom.spec.whatwg.org/ |
| DOM | Web IDL | https://webidl.spec.whatwg.org/ |
| CSS | CSSWG draft modules | https://drafts.csswg.org/ |
| CSS | Property / selector / at-rule indexes | https://drafts.csswg.org/indexes/ |
| JS | ECMA-262 (ECMAScript language) | https://tc39.es/ecma262/ |
| JS | ECMA-402 (Intl) | https://tc39.es/ecma402/ |

### Machine-readable datasets used for the measurements

| Dataset | Version | Contents |
|---|---|---|
| `@webref/css` | 8.7.8 | 822 properties, 159 selectors, 55 at-rules, 172 functions, 529 types |
| `@webref/elements` | 2.9.2 | HTML (142), SVG 1.1 (62), SVG 2 (30), MathML Core (31) elements |
| `@webref/idl` | 3.86.0 | 346 Web IDL files; 1,251 interfaces (1,643 declarations incl. partials) |
| `@mdn/browser-compat-data` | 8.1.8 | Shipping-engine support data, used as a denominator check |

### Reference points (roles, not dependencies)

| Role | Use here |
|---|---|
| Production HTML parser | sanity check for tree-construction corner cases |
| Production CSS parse/match/cascade stack | sanity check for selector and cascade semantics |
| Independent JavaScript engine (optional `engine-b`) | differential check of script results |
| DOM implementation in script | cross-check of DOM wrapper behaviour |
| Web conformance suites | pass-rate measurement (not yet run) |
| ECMAScript conformance suite | pass-rate measurement (not yet run) |

None of these is a dependency except the optional `engine-b` runtime. The main
engine is from scratch.

---

## 2. Scorecard

| Layer | Implemented | Dataset surface | Nominal coverage | Notes |
|---|---|---:|---:|---|
| CSS property names | 171 in `KNOWN` | 752 non-vendor | ~22% | dataset includes drafts; ~25-27% against names shipping in two mainstream engines |
| CSS selectors | ~30 forms | 159 | ~20-25% | exact list in Â§4.2 |
| CSS at-rules | 5 | 55 | ~9% | most missing ones are draft, print, speech, or animation-only |
| HTML elements | 142 parse | 142 | behaviour partial | generic fallback parses all; 14 live elements lack behaviour |
| Web IDL interfaces | ~8 families | 1,251 | <1% | denominator dominated by GPU, audio, XR, workers |
| DOM methods bound to JS | 79 | - | - | common tree, event, observer, style, and form surface only |
| JS language globals | 48 | ~50 | ~96% | strongest layer; `Proxy`, `Intl`, `eval` absent; `Reflect` ships (wave 76) |
| test262 | not run | ~50k | unknown | highest-value missing measurement |
| WPT | not run | ~50k | unknown | needs a harness; targeted subsets only |

The important asymmetry is not 94% versus 16%. It is that the JavaScript
language core is the strongest layer, the Web API layer is deliberately thin, and
the CSS number mixes shipping properties with drafts. A general-purpose parity
target would fight the RAM metric; see Â§8.

Removed from the previous draft: the "~12% overall parity" line, which had no
source in `PLAN.md`.

---

## 3. HTML

- **142 elements** are defined by the HTML Living Standard.
- The tokenizer/parser is generic: any unknown tag becomes a normal element, so
  parsing coverage is effectively complete.
- `<noscript>` content is raw text, because scripting is always enabled: a
  fallback `<style>` or `<meta http-equiv=refresh>` inside it never takes
  effect (wave 77, found on google.com).
- **32 real tag names** have no element-specific code path (the raw diff had 33
  names; `multicol` is a non-standard leftover).
- Of those 32: 15 are obsolete or removed, 2 need no branch at all, 1 is legacy
  but live, and 14 are live behavioural gaps.

### No tag branch: classification

```text
Obsolete / removed (no work needed):
  acronym, applet, basefont, bgsound, blink, frame, frameset, isindex,
  keygen, marquee, menuitem, nextid, noembed, noframes, spacer

No branch needed (generic inline rendering is correct):
  abbr, dfn

Legacy but live:
  nobr (the only behaviour is nowrap)

Live behavioural gaps:
  bdi, bdo, canvas, datalist, iframe, meter, output,
  progress, rb, rp, rt, rtc, ruby, selectedcontent
```

The previous draft counted `abbr` and `dfn` as gaps; they render correctly as
inline elements and need no special code. `bdi`/`bdo` are real gaps because the
engine has no bidi algorithm or `direction` handling.

### Notable behavioural gaps

| Area | Elements | Status |
|---|---|---|
| Graphics | `<canvas>` | no element behaviour, no 2D API, no backing store |
| Embeds | `<iframe>`, `<object>`, `<embed>` | no nested browsing context; `object`/`embed` named only |
| Media | `<video>`, `<audio>`, `<source>`, `<track>` | labelled placeholder; `play()` rejects |
| Widgets | `<dialog>`, `<details>`/`<summary>`, `<meter>`, `<progress>`, `<datalist>`, `<output>` | no behaviour |
| Ruby | `<ruby>`, `<rt>`, `<rp>`, `<rb>`, `<rtc>` | not handled |
| Bidi | `<bdi>`, `<bdo>` | no bidi algorithm |
| Images | `<picture>` | named, but `srcset`/`source` resolution is partial |
| Scripting | `<template>`, `<slot>` | declarative shadow DOM only; no imperative shadow DOM |
| MathML | `<math>` and MathML Core elements | not implemented |

### What is supported

- Block and inline flow for all generic elements.
- UA box defaults: list gutter for `ul`/`ol`, table roles and spacing, cell
  padding, `sub`/`sup` baseline shifts. Inherited defaults cover headings, links,
  `pre`, and list types.
- Tables: `border-collapse`, `border-spacing`, `table-layout: fixed`.
- Forms: `input` (text/checkbox/radio/submit/button), `button`, `select`/`option`,
  `textarea`, `fieldset`/`legend`, `label` (partial).
- Images: PNG, JPEG, WebP, SVG (hand-written rasterizer); `srcset` partial.
  A replaced element's intrinsic size is its content's; `width`/`height`
  attributes, `max-width`/`min-width` and CSS sizes clamp it.
- Declarative shadow DOM, inline `<svg>`, `<script>`/`<style>`/`<link>`/`<meta>`.
  Inline SVG sizing follows the intrinsic rules above: the root `viewBox` is a
  coordinate system (its ratio sizes an auto axis) and a viewBox-only root
  defaults to `width: 100%` of its containing block, height from the ratio.

---

## 4. CSS

### 4.1 Property names

| Metric | Count |
|---|---:|
| Properties in `@webref/css` | 822 |
| Non-vendor properties | 752 |
| Names in the engine's `KNOWN` table | 171 |
| Additional accepted alias | `word-wrap` (same handler as `overflow-wrap`, also listed) |
| Raw missing (non-vendor) | ~582 |
| Coverage (non-vendor) | ~22% |
| Coverage against names shipping in two mainstream engines | ~25-27% |

The `KNOWN` table is the source of truth for this count. A name in the table can
still be only partially implemented; a name outside it can still work through a
shorthand. For example, `border-radius` and all four corner longhands are
handled, but the paint model still applies one radius to every corner.

### Coverage by category (raw dataset diff)

| Category | Missing | Examples | Decision relevance |
|---|---:|---|---|
| Other / miscellaneous | 217 | `bookmark-label`, `block-step-align`, `corner-*`, `cue` | ignore: drafts, print, speech |
| Text & typography | 60 | `hyphens` | high: frequent in real CSS |
| Logical properties & writing modes | 61 | `block-size`, `inline-size`, `margin-trim` | low frequency; wave 81 maps the horizontal-tb/ltr subset to physical |
| Animation & transitions | 46 | `animation-*`, `transition-*` | medium only if motion is required |
| Borders & outlines | 18 | `border-image-*`, `border-block-*`, `border-inline-*` | medium: common in resets |
| Scrolling | 33 | `scroll-behavior`, `scrollbar-*`, `overscroll-behavior` | low: static captures |
| Paged & multi-column | 31 | `break-*`, `column-*` | low |
| Fonts | 22 | `font-variant-*`, `font-kerning`, `font-stretch` | medium |
| Grid & alignment | 9 | `grid-row`, `grid-auto-rows` | high |
| Transforms & motion paths | 11 | `perspective`, `offset-*`, `transform-style` | medium |
| Overflow | 14 | `overflow-anchor`, `overflow-clip-margin-*` | low |
| Backgrounds | 12 | `background-origin`, `background-attachment`, `background-blend-mode` | medium |
| Masking & clipping | 13 | `mask-border-*`, `mask-mode`, `mask-composite` | low |
| Containment, containers & anchors | 13 | `contain`, `container-*`, `anchor-*` | low |
| Box, display & positioning | 11 | `appearance`, `all`, `float-offset` | medium |
| Color & theming | 9 | `accent-color`, `caret-color`, `caret-shape` | low-medium |
| Lists | 6 | `list-style-image`, `marker-*` | low |
| Filters & compositing | 3 | `mix-blend-mode` | high for visual parity |
| Tables | 0 | - | low |
| Flexbox | 1 | `flex-line-count` | low |

### 4.2 Selectors

Implemented:

- Type, `.class`, `#id`.
- Attribute selectors: `[a]`, `[a=v]`, `[a~=v]`, `[a|=v]`, `[a^=v]`,
  `[a$=v]`, `[a*=v]`.
- Combinators: descendant, child (`>`), adjacent sibling (`+`).
- Structural pseudo-classes: `:first-child`, `:last-child`, `:first-of-type`,
  `:last-of-type`, `:only-child`, `:nth-child()`, `:nth-last-child()`,
  `:nth-of-type()`, `:nth-last-of-type()`.
- `:is()`, `:where()`, `:not()` (including lists), `:has()`.
- `:root`, `:link`, `:enabled`, `:disabled`.
- `:hover` through the sheet's hover cell; `:visited` matches nothing (no visit
  history is modeled).
- `:host(...)` bound to the component that declared the style (declarative
  shadow DOM).
- Pseudo-elements: `::before`, `::after`, `::marker`.

Dropped at parse time (a rule containing one of these never applies):

```text
:focus, :focus-visible, :focus-within, :active, :checked, :target,
:default, :required, :optional, :indeterminate, :valid, :invalid,
:in-range, :out-of-range, :read-only, :read-write, :placeholder-shown
```

This is not "never matches": the whole selector is rejected, so the rule is
absent. The exception is inside `:not(...)`, where a dynamic member is ignored
(vacuously true), which is what makes `:not(:focus)` styling work.

Missing:

```text
general-sibling (~) combinator outside :has(), :empty, :only-of-type,
:lang(), :dir(), :host-context(), ::first-line, ::first-letter,
::selection, ::placeholder, ::backdrop
```

This corrects the previous draft, which listed `:root`, `:nth-of-type`,
`:nth-last-child`, `:only-child`, `:checked`, the attribute operators, and
`:host(...)` as missing.

### 4.3 At-rules

Implemented: `@media`, `@supports`, `@layer`, `@font-face`, `@import`.

Missing: `@keyframes`, `@container`, `@scope`, `@page`, `@property`,
`@counter-style`, `@font-feature-values`, `@font-palette-values`,
`@starting-style`, `@position-try`, `@view-transition`, `@namespace`,
`@charset`.

Most of the missing set is draft, print, or animation-only. `@keyframes` becomes
relevant only when transitions and animations exist.

### 4.4 Behavioural gaps that affect real pages

- Backgrounds support a single `linear-gradient(...)` or
  `radial-gradient(...)` layer (angles/corner keywords or centre/shape/extent
  sizing, percentage stops, hard stops and alpha); `conic-gradient`,
  multi-layer stacks, `repeating-*` tiling and `background-origin` are absent.
  No animations or transitions, no `@keyframes`.
- `@media` only understands width, hover, pointer, orientation, and
  `prefers-color-scheme`.
- Grid: no `auto-fill`/`auto-fit`, no `min-content`/`max-content`, `repeat()`
  capped at 64; `place-items`/`place-self` map to the existing
  `align-items`/`justify-items` and `align-self`/`justify-self` fields
  (one value covers both axes, a second value sets the inline axis, and
  `place-self: auto` clears the override). `justify-items`/`justify-self`
  align grid items inside their area, and `align-self` now shifts a grid
  item within its row as it already did in flex; `align-content`
  distributes rows, but `stretch` packs at the start (auto rows never grow).
- Flex: `wrap-reverse` does not reverse the cross axis. A `width: %` item's
  basis is the percentage resolved against the row (no intrinsic floor), so
  several `width:100%` siblings shrink evenly via `flex-shrink`; absolute
  min/max widths still clamp after shrink. A `%` `max-`/`min-width` on an item
  is pinned against the row before measuring (it is not re-resolved against
  the item's own slot), the automatic minimum size is clamped by
  `max-`/`min-width`, and shrink is weighted by the content base rather than
  the margin box, so equal columns stay equal when one carries side margins.
  Min-content and the line breaker honour UAX #14 hyphen break opportunities
  (the longest hyphen-free piece is the floor), but `hyphens: auto`
  dictionary/`soft hyphen` hyphenation is still not modeled.
- Margins: the UA sheet supplies the browser defaults (`p`/`dl`/`pre`/
  `address`/`ul`/`ol` 1em block, `blockquote`/`figure` 1em block + 40px
  inline, `h1` 0.67em … `h6` 2.33em, `dd` 40px start, `body` 8px) only on the
  sides no author declaration claimed, after the cascade and the final
  font-size are known. Adjacent in-flow siblings collapse (positive max,
  negative min, mixed sum), but a parent's first/last child margin is kept
  inside the parent instead of collapsing through a padding/border-free
  ancestor - `hr` is still a hard-break leaf with no UA margin.
- Logical properties: `inline-size`/`block-size`, their min/max forms,
  `inset-inline-*`/`inset-block-*`, `margin`/`padding`/`border` `-before`/
  `-after`/`-start`/`-end` aliases and the two-sided `inset-inline`,
  `inset-block`, `border-inline`/`border-block` (+ width/style/color)
  shorthands map to physical left/right/top/bottom. Only horizontal-tb with
  `direction: ltr` is modeled; writing modes and bidi are still absent.
- `text-wrap: nowrap` shares the inherited no-wrap mode with
  `white-space: nowrap`; `appearance: none` suppresses UA control chrome;
  `background-position-x`/`-y` replace one axis. Interaction-only properties
  (`pointer-events`, `user-select`, scroll margins/padding, …) are accepted
  no-ops.
- Borders: per-side width and colour longhands parse into the existing model,
  which still keeps one colour for all sides and one radius for all corners
  (the last declaration wins); a `transparent` colour keeps the side's width
  but paints nothing; style words collapse to a solid border;
  dashed/dotted text decorations are approximated; inner shadows are dropped.
- `%` `min-height` / `max-height` resolve against width, not container
  height, and a `%` `height` is still ignored (the engine does not track the
  containing block's height). A px-resolved `height` now sets an auto block's
  used content height (content taller than it overflows), which is what
  `.highlight { height: 12px }` bars need.
- `line-clamp` keeps the first N text line boxes, adds a trailing marker and
  shrinks the box; it still lays out as block flow (a vendor-prefixed
  `display: box` is an unknown display value), counts nested
  block text, and truncates text only (child boxes/images below the cut rely
  on `overflow` clipping).
- `outline` paints a solid ring only (styles collapse, `auto` is off);
  `clip-path` is `inset()` only.
- `opacity` is applied per painted primitive (backgrounds, borders, images,
  text, decorations), not as a group composite: overlapping translucent
  children of the same box can show through each other. `opacity: 0` removes
  the subtree.
- Interaction pseudo-classes (`:focus`, `:focus-visible`, `:checked`,
  `:active`) drop the whole rule; only `:hover` and `:not(:focus)` survive.
  Focus rings and custom checkbox states are therefore missing.
- `var()` is a flat page-wide map, not a per-element cascade.
- The `font` shorthand sets size, line-height, style, weight and family and
  resets the components it omits; its `font-variant-*`/`font-stretch`
  components are accepted but ignored, and `font: inherit`/`unset` plus the
  system keywords are no-ops (they don't revert an earlier same-element
  declaration). `font-size` also accepts the absolute-size keywords
  (`xx-small`â€¦`xxx-large`, `larger`, `smaller`), and `line-height` accepts
  unitless, px/em/rem/%, viewport units and `normal`.
- Text is sized by the CSS em square, not the font's ascent-to-descent height:
  glyphs are scaled by `size × height_unscaled / units_per_em` and the
  baseline is the real ascent plus CSS half-leading. `rem` resolves against
  the `<html>` element's computed `font-size` (not a fixed UA default), and a
  unitless `line-height` is a factor applied to the element's final
  font-size. `em` on a property resolves against the element's own
  font-size.
- `filter` supports the shorthand colour functions (`grayscale`, `sepia`,
  `saturate`, `hue-rotate`, `invert`, `opacity`, `brightness`, `contrast`)
  collapsed into one affine RGB matrix plus an alpha multiplier. `blur`,
  `drop-shadow` and `url()` references are accepted but have no effect, and
  the result is applied per primitive (own background/border/outline/shadow/
  gradient/image and text ink) rather than as a real offscreen group, so
  overlapping children of a filtered parent can show through each other.
- `background-clip` supports the three box values (`border-box`,
  `padding-box`, `content-box`) for the colour and gradient layers and crops
  the background image for `content-box`; `background-clip: text` is not
  accepted (it needs a glyph-shaped mask this renderer does not have).
- `backdrop-filter` blurs the already-painted pixels behind a box's border
  box: the subtree's paint entries are deferred between two paint phases,
  the region is box-blurred (three passes ≈ Gaussian, σ = blur) and the
  subtree repaints sharp on top. Only `blur()` contributes (the other
  filter functions parse but are inert), the blur does not scale with the
  box's transform, and a `position: fixed` descendant paints in the fixed
  pass without its ancestor's blur.
- `transform-origin` is honored for the translate/scale composite formed by
  `transform`, `translate` and `scale` (no 3D; `rotate` parses but paints
  unrotated, and glyph size still does not scale â€” see the wave 53 notes);
  `overflow-wrap` / `word-break` now break inside words during line layout,
  `text-overflow: ellipsis` marks a clipped line and `max-height` caps a box
  (see the wave 49-52 notes); `hyphens` is not modeled.
- `text-shadow` paints up to TWO layers per run (the style field is a fixed
  array so `Style` stays `Copy`), each offset + box-blurred (three passes ≈
  Gaussian, σ = blur/2) under the glyphs; offsets are rounded to whole
  pixels and the underline/strike/overline decorations are not shadowed.
- `grid-auto-flow: [row|column] || dense` picks the auto-placement axis;
  `dense` parses but is not stored (the sparse cursor never passes a free
  cell while auto items are 1×1 and row spans don't exist), and column flow
  grows the grid with implicit `auto` columns past the explicit template.
- `direction`, `unicode-bidi`, and writing modes are absent.

---

## 5. DOM and Web APIs

`@webref/idl` (3.86.0) contains **346 IDL files, 1,251 interfaces**, plus 1,117
dictionaries, 399 enums, 78 callbacks and 150 typedefs.

The engine binds about **76 native methods** across roughly these interfaces:

`Window`, `Document`, `Node`, `Element`, `Event`, `MutationObserver`,
`CSSStyleDeclaration`,
`classList`, `history`, `location`, `navigator`, `localStorage` /
`sessionStorage`, `MediaQueryList`, `fetch` / `Response`.

That is under 1% of the interface surface, but the denominator is not a useful
target: most of it is GPU, audio, XR, workers, and storage APIs that no reference
page needs.

### Missing API families

| Family | IDL file | Interfaces | Status |
|---|---|---:|---|
| HTML element APIs + Canvas | `html.idl` | 240 | ~8 implemented |
| DOM core (observers, ranges, collections) | `dom.idl` | 42 | partial |
| Web Audio | `webaudio.idl` | 39 | absent |
| WebGPU | `webgpu.idl` | 46 | absent |
| CSSOM | `cssom.idl` | 20 | `CSSStyleDeclaration` only |
| Service Workers | `service-workers.idl` | 17 | absent |
| Streams | `streams.idl` | 15 | absent |
| IndexedDB | `indexeddb.idl` | 13 | absent |
| UI Events | `uievents.idl` | 11 | partial |
| Fetch | `fetch.idl` | 7 | `fetch` + `text()` only |
| File API | `FileAPI.idl` | 6 | absent |
| Encoding | `encoding.idl` | 6 | `TextEncoder` / `TextDecoder` |
| XMLHttpRequest | `xhr.idl` | 5 | absent |
| Notifications | `notifications.idl` | 4 | absent |
| Performance Timeline | `performance-timeline.idl` | 4 | partial |
| WebSocket | `websockets.idl` | 2 | absent |
| URL | `url.idl` | 2 | `URL`, `URLSearchParams` |

Notably absent:

```text
IntersectionObserver, ResizeObserver, Worker, WebSocket,
XMLHttpRequest, IndexedDB, crypto.subtle, requestIdleCallback, structuredClone,
dataset writes (reads work, writes do not update the attribute)
```

`document.forms`, `getElementsByName`, `createTextNode`, `createComment`,
`Node.childNodes` / `Element.children`, `queueMicrotask`, `DataView`,
`AbortController` / `AbortSignal`, the `Event` / `CustomEvent`
constructors with synchronous `dispatchEvent`, and `MutationObserver` now
exist. `getElementsByTagName` / `getElementsByClassName` /
`getElementsByName`, `document.forms` and `form.elements` are LIVE
collections (`length`, index, `item()`, `namedItem()`, named property
reads and iteration re-query the DOM); `querySelectorAll` is a static
NodeList, as in a browser. `form.controlName` shorthand on the form
wrapper itself is still absent (use `form.elements`). Console-created
(`new`) events bubble through elements,
document and window and honour both stop flags; shell-created events still
lack `stopPropagation`. Mutation records cover `childList`, `attributes`
and `characterData` (with `subtree`, old-value flags and
`attributeFilter`); callbacks run once at the end of a script turn rather
than at every microtask checkpoint, one target per observer, and
`previousSibling` / `nextSibling` are null.

Correction: `sessionStorage` is **present**. It is deliberately merged with
`localStorage` into one in-memory store (`src/script.rs`), and neither is backed
to disk.

---

## 6. JavaScript language

47 globals are installed; ECMA-262 defines roughly 50 standard globals.

### Present

`Object`, `Function`, `Array`, `String`, `Number`, `Boolean`, `BigInt`,
`Symbol`, `Date`, `RegExp`, `Promise`, `Map`, `Set`, `WeakMap`, `WeakSet`,
`JSON`, `Math`, `ArrayBuffer`, `SharedArrayBuffer`, `DataView`, typed arrays
(`Int8Array` ... `BigUint64Array`), the `Error` family, `TextEncoder`,
`TextDecoder`, `URL`, `URLSearchParams`, `AbortController` / `AbortSignal`,
`setTimeout` / `setInterval` / `clearTimeout` / `clearInterval`,
`queueMicrotask`, `console`,
`parseInt` / `parseFloat` / `isNaN` / `isFinite`,
`encodeURIComponent` / `decodeURIComponent`, `atob` / `btoa`, `globalThis`,
`window`, `self`, `performance`.

### Missing

`Proxy`, `Reflect`, `Intl`, `WeakRef`, `FinalizationRegistry`,
`Atomics`, `eval()`, `encodeURI`, `decodeURI`, `escape` / `unescape`,
`structuredClone`.

The global count hides missing prototype methods. Verified absent examples:
`Array.prototype.at`, `Array.prototype.findLast`, `Array.prototype.toSorted`,
`Object.groupBy`, `String.prototype.replaceAll`.

### Language-feature notes

- Classes, modules, dynamic `import()`, promises, `new.target`, private fields,
  and private brand checks are implemented.
- Generators run through a mini-VM rather than a full spec state machine; async
  is built on the same machinery.
- No JIT: it is a tree-walking interpreter, so throughput is the main limit on
  heavy JavaScript applications.
- `Buffer` is a Node-style compatibility shim, not part of ECMA-262.

---

## 7. Other engine gaps

| Area | Status |
|---|---|
| Rendering | CPU software rasterizer only; no GPU compositing |
| Media | no video/audio decoding; DRM parked |
| Fonts | WOFF1 supported; WOFF2 feature-gated (`--features woff2`); no per-stack fallback |
| Images | PNG, JPEG, WebP, SVG; no AVIF, no GIF |
| Networking | HTTP/1.1 only; no HTTP/2; TLS profile is a modern mainstream shape, not a fingerprint clone |
| Process model | one process, no sandbox |
| DevTools | element inspector only; no console or network debugger |
| Extensions | none |
| Conformance tests | none - 879 self-authored unit tests (882 with `engine-b + woff2`), zero conformance-suite runs |

---

## 8. Turning this into decisions

### 8.1 Denominators that mean something

Surface coverage against a full spec dataset is a proxy that rewards
implementing drafts. Use one of these instead:

1. **Reference-site denominator:** the union of CSS declarations, DOM members,
   and JavaScript APIs actually touched by the seven reference pages. This is the
   only denominator that directly measures the success criterion.
2. **Shipping denominator:** features that at least two mainstream engines ship.
   Label single-engine and draft-only features and keep them out of headline
   numbers.

The best signal is neither: instrument the engine to log unresolved property
names and missing DOM/JS members during a reference run, then rank by frequency
and visual impact. That turns this document from a wish list into a work queue.
Wave 66 did the CSS half (`MAZDA_MISS=1`); the ranked output (transitions,
`filter`/`backdrop-filter`, `place-items`) now orders the CSS queue, and
waves 67-74 took the top visual items off it (individual `translate`/`scale`,
the `font` shorthand, the `filter` colour functions, the `place-items`/
`place-self` alignment shorthands, `background-clip`, `text-shadow`,
`grid-auto-flow`, then `backdrop-filter`).
Wave 75 did the JS half: the same flag now ranks unresolved JavaScript names
in two buckets (missing globals vs reads on engine-built wrappers), 512
distinct names each with an over-cap tally. First census: nextjs.org 61
globals / 58 members (top globals `process` ×6, `Reflect` ×5; top members a
page-built `Symbol(…)_<n>` key ×8, `@@iterator` ×4, `deleteAll` ×4 — the
page-built keys and the one-shot environment probes sit on top, so the
actionable queue starts below them), MDN 1/0, Wikipedia 2/1, Hacker News 0/1
(`innerHTML` as a read on element wrappers), rust-lang.org 1/0. The ranked
lists are the JS work queue; wave 76 took its top global, `Reflect` (nextjs
globals 61 → 60, members 58 → 67 because core-js's decorator-metadata module
now probes `Reflect.defineMetadata`/`getMetadata`/…), and wave 77 took the
Array extras (`find`/`findIndex`/`flat`/`flatMap`/`at`/`fill`/`copyWithin` and
`findLast*`; nextjs members 67 → 61 with no new names). google.com joined the
reference set in wave 77 and immediately paid for itself: its first capture
was blank because `<noscript>` content was parsed as elements, so its
non-JS `<style>` hid the page. Wave 78 then fixed the three visual defects the
baseline exposed — viewBox numbers read as pixels, the flex `%` basis
floor/cap, and transparent borders painted as currentColor — taking it from
1400px to 488px / 28 runs / 92.3 MB (Chrome: 525px), form 56px, icons
36/24/20px; `[css-miss]` 50 and `[js-miss]` 20 globals + 11 members
(`XMLHttpRequest`, `trustedTypes`, `fonts.timing`, `sendBeacon`, …) are
unchanged. Wave 79 fixed the text itself: `ab_glyph`'s `PxScale` targets the
font height, not the em square, so every run had been laid out and painted at
`em/height` (Fira 5/6, Alfa 0.73); `em_scale` now uses
`size × height_unscaled / units_per_em` with a real-ascent baseline, `rem`
resolves against the `<html>` computed font-size, and a unitless
`line-height` is a factor of the final font-size. Chrome canvas
`measureText` on the live webfonts is the ground truth (`Install@18`
51.10px, `Governance@18` 96.25px, `empowering@36` 201.82px; Alfa `Rust@18`
44.21px) and the engine matches. rust-lang.org drops 5344 -> 4113px against
Chrome's 3841px, google 488 -> 539px (Chrome fits it in the 616px viewport),
nextjs 7906 (Chrome 10548), MDN 2966 (3328), Hacker News 1782 (1214).
Wikipedia's 28074px has no Chrome number this wave: headless Chrome served a
Wikimedia error page. Wave 80 then removed the synthetic 0.6em paragraph gap
and gave the cascade real UA margins plus sibling collapsing: Wikipedia
28074 -> 21872px (its `p { margin: 0.5em 0 }` boundaries were 25.6px, now 8),
nextjs 7906 -> 7810, rust-lang.org 4113 -> 3572 (Chrome 3841 — the remaining
~269px is section spacing, not paragraph margins), MDN 2966 -> 2925 (3328),
Hacker News 1782 -> 1772 (1217) with Chrome's 8px body inset, and google
539 -> 514. Wave 81 then closed the logical-property cluster: `inline-size`/
`block-size`, `inset-inline-start`, the legacy `margin-before`/`start`/`end`
spellings and the two-sided `inset-inline`/`border-block` shorthands now map
to their physical left/right/top/bottom properties before the `KNOWN` gate
(horizontal-tb + ltr only), `text-wrap: nowrap` joins the no-wrap mode,
`appearance: none` suppresses native-control chrome, and
`background-position-x`/`-y` replace one axis. The `MAZDA_MISS` census fell
from 50 to 16 distinct dropped properties on google, 84 to 43 on nextjs and
12 to 4 on MDN; heights barely move (MDN 2925 -> 2926, the rest unchanged)
because the declarations sit on icons and decorative boxes. What remains:
`Reflect`
metadata methods, ES2024 Set methods, Map `emplace`/`upsert`,
`Object.isExtensible`/`getOwnPropertySymbols`, `reportError`, `innerHTML`
reads, and the section/heading spacing rules that keep rust-lang.org short.

### 8.2 Priority by reference impact and RAM cost

| Priority | Work | Why | Reference impact | RAM cost | Effort |
|---|---|---|---|---|---|
| P0 (done) | Observed-miss instrumentation for CSS names and DOM/JS member reads | replaces speculation with a ranked list; wave 66 ships the CSS half (`MAZDA_MISS=1`) and uses it to add the border longhands; wave 75 ships the JS half (ranked `[js-miss]` global/member buckets) | all seven | negligible (dev-only) | done (wave 66 CSS, wave 75 JS) |
| P0 (done) | Shrink JS AST/env/object types (waves 63-64: `Obj` 88 to 64 B, `Param` 144 to 40 B, `Stmt` 192 to 72 B, `Closure` 112 to 96 B) | directly serves the success metric; wave 44 saved 36 MB on one workload and waves 63-64 cut ~5 MB off nextjs.org | all | negative (saves) | done |
| P1 | `form.controlName` shorthand on the form wrapper | some libraries read controls directly off the form | some pages | low | 1 wave |
| P2 | `mix-blend-mode`, transitions/`@keyframes` | visual fidelity on modern marketing pages | verify against captures | medium | 2-4 waves |
| P3 | `<canvas>` 2D, `<iframe>`, `<video>`/`<audio>` | large new subsystems; only if a target page changes | none of the seven | high | weeks-months |
| P3 | WebGPU, Web Audio, IndexedDB, workers, WebSocket | huge surface, not needed for the captures | none | very high | no |

`<canvas>` is not just an element: it needs a 2D path/text/image API and a pixel
backing store, which is a large RAM commitment. Do not add it for a checkbox.

### 8.3 Conformance harnesses

1. **ECMAScript conformance suite.** Highest value because the language core is
   the strongest layer. It needs host hooks, `print`, module/async flags, and
   negative-test handling. Start with the `language/` subset; record pass/fail;
   exclude staging tests. Budget a harness project, not a wave.
2. **Web conformance suite.** Much heavier: a local server, test driver, async
   tests, and reftests. A curated subset (`css/selectors/`, `dom/nodes/`,
   `html/dom/`) is the only realistic first step. Screenshot diffing against the
   seven reference captures is more aligned with this project.
3. **Shipping-feature denominator.** Intersect the implemented surface with
   features that at least two mainstream engines ship, to exclude drafts.

### 8.4 Measurement recipe

- Source is the source of truth: count `KNOWN` entries and match arms, not
  README prose.
- For memory, use a deterministic synthetic workload and report the working-set
  delta; live RSS noise is often larger than the effect.
- For fidelity, run the reference URL, capture height, run count, working set,
  and script errors, and compare against the previous wave.
- Never synthesize mouse or keyboard input. Use PrintWindow captures and dump
  modes.
- Every number in a report needs a command or a stated estimate.

---

## Appendix A - CSS property names accepted (171)

```text
align-content, align-items, align-self, aspect-ratio
backdrop-filter, background, background-clip, background-color, background-image, background-position, background-repeat
background-size, border, border-bottom, border-bottom-color
border-bottom-left-radius, border-bottom-right-radius, border-bottom-style
border-bottom-width, border-collapse, border-color, border-left
border-left-color, border-left-style, border-left-width, border-radius
border-right, border-right-color, border-right-style, border-right-width
border-spacing, border-style, border-top, border-top-color
border-top-left-radius, border-top-right-radius, border-top-style
border-top-width, border-width, bottom
box-shadow, box-sizing, caption-side, clear, clip
clip-path, color, color-scheme, column-gap
content, cursor, display, fill
fill-opacity, fill-rule, filter, flex, flex-basis
flex-direction, flex-flow, flex-grow, flex-shrink
flex-wrap, float, font, font-family, font-size
font-style, font-weight, gap, grid-area, grid-auto-flow
grid-column, grid-column-end, grid-column-start, grid-template-areas
grid-template-columns, grid-template-rows, height, inset
justify-content, justify-items, justify-self, left, letter-spacing, line-clamp
line-height, list-style, list-style-position, list-style-type, margin
margin-block, margin-block-end, margin-block-start, margin-bottom
margin-inline, margin-inline-end, margin-inline-start, margin-left
margin-right, margin-top, mask, mask-image
mask-position, mask-repeat, mask-size, max-height
max-width, min-height, min-width, object-fit
object-position, opacity, order, outline
outline-color, outline-offset, outline-style, outline-width
overflow, overflow-wrap, overflow-x, overflow-y
padding, padding-block, padding-block-end, padding-block-start
padding-bottom, padding-inline, padding-inline-end, padding-inline-start
padding-left, padding-right, padding-top, place-content, place-items, place-self
position, right, row-gap, stroke, stroke-linecap
stroke-linejoin, stroke-opacity, stroke-width, table-layout
text-align, text-decoration, text-decoration-color, text-decoration-line
text-decoration-style, text-decoration-thickness, text-indent, text-overflow
text-shadow, text-transform, text-underline-offset, top, transform
transform-origin, translate, rotate, scale, vertical-align, visibility
white-space, width, word-break, word-spacing, word-wrap
z-index
```

`word-wrap` is the legacy spelling of `overflow-wrap`; both names are in
`KNOWN` and run the same handler.

## Appendix B - Missing CSS names worth considering

This is a curated shipping-relevant subset, not the raw 630-name dataset dump.
The full list is reproducible with Appendix D.

```text
Layout and alignment:
  grid-auto-columns, grid-auto-rows, grid-row,
  grid-row-start, grid-row-end, grid-template, grid-gap

Box, border, outline:
  border-image-*, border-block-*, border-inline-*, box-decoration-break

Text and typography:
  hyphens, text-emphasis, tab-size, quotes,
  direction, unicode-bidi, writing-mode, text-orientation,
  white-space-collapse, text-wrap, font-stretch, font-variant,
  font-feature-settings, font-kerning, font-size-adjust, font-optical-sizing

Backgrounds and effects:
  background-origin, background-attachment,
  background-blend-mode, mix-blend-mode, isolation

Transforms and motion:
  transform-style, perspective, perspective-origin, backface-visibility, transition,
  transition-delay, transition-duration, transition-property,
  transition-timing-function, animation*, @keyframes (at-rule)

Scrolling and interaction:
  scroll-behavior, scroll-snap-type, scroll-snap-align, overscroll-behavior,
  pointer-events, user-select, touch-action, caret-color, accent-color,
  appearance

Tables, columns, generated content:
  empty-cells, column-count, column-width, column-fill,
  column-span, column-rule, columns, counter-increment, counter-reset,
  counter-set, list-style-image, marker-start, marker-mid, marker-end

SVG:
  stop-color, stop-opacity, flood-color, flood-opacity, stroke-dasharray,
  stroke-dashoffset, stroke-miterlimit, marker-start, marker-mid, marker-end,
  paint-order, vector-effect, text-anchor, dominant-baseline,
  color-interpolation-filters, clip-rule

Deliberately out of scope (draft, obsolete, print, or speech):
  bookmark-*, block-step-*, corner-*, cue, voice, speak, timeline-trigger-*,
  page/break-*, @page, @charset, @namespace, and most of the 217
  "other/miscellaneous" bucket
```

## Appendix C - HTML elements without a tag branch

```text
Obsolete / removed:
  acronym, applet, basefont, bgsound, blink, frame, frameset, isindex,
  keygen, marquee, menuitem, nextid, noembed, noframes, spacer

No branch needed:
  abbr, dfn

Legacy but live:
  nobr

Live gaps:
  bdi, bdo, canvas, datalist, iframe, meter, output,
  progress, rb, rp, rt, rtc, ruby, selectedcontent

Non-standard leftover in the dataset:
  multicol
```

## Appendix D - Sources and reproducibility

All counts were produced from the datasets below. The engine-side counts come
from the source, not from the documentation.

```powershell
# CSS: 822 total, 752 non-vendor
Invoke-WebRequest https://unpkg.com/@webref/css/css.json -OutFile css.json
$webref = Get-Content css.json -Raw | ConvertFrom-Json
$webref.properties.name.Count
($webref.properties.name | Where-Object { $_ -notlike '-*' }).Count

# HTML: 142 elements
Invoke-WebRequest https://unpkg.com/@webref/elements@2.9.2/html.json -OutFile html.json
(Get-Content html.json -Raw | ConvertFrom-Json).elements.name.Count

# Web APIs: 346 IDL files; count "interface" declarations
Invoke-WebRequest "https://unpkg.com/@webref/idl/?meta" -OutFile idl-meta.json
(Get-Content idl-meta.json -Raw | ConvertFrom-Json).files.Count

# Engine property table: 171 names
Select-String -Path src\css.rs -Pattern 'const KNOWN' -Context 0,130
```

Dataset versions at the time of measurement: `@webref/css` 8.7.8,
`@webref/elements` 2.9.2, `@webref/idl` 3.86.0, `@mdn/browser-compat-data` 8.1.8.
