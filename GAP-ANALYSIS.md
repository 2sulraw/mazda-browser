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
> metric is steady-state RAM plus six reference captures; §8 is the part that
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
| CSS property names | 142 in `KNOWN` + 1 alias | 752 non-vendor | ~18% | dataset includes drafts; ~21-23% against names shipping in two mainstream engines |
| CSS selectors | ~30 forms | 159 | ~20-25% | exact list in §4.2 |
| CSS at-rules | 5 | 55 | ~9% | most missing ones are draft, print, speech, or animation-only |
| HTML elements | 142 parse | 142 | behaviour partial | generic fallback parses all; 14 live elements lack behaviour |
| Web IDL interfaces | ~8 families | 1,251 | <1% | denominator dominated by GPU, audio, XR, workers |
| DOM methods bound to JS | 79 | - | - | common tree, event, observer, style, and form surface only |
| JS language globals | 47 | ~50 | ~94% | strongest layer; `Proxy`, `Reflect`, `Intl`, `eval` absent |
| test262 | not run | ~50k | unknown | highest-value missing measurement |
| WPT | not run | ~50k | unknown | needs a harness; targeted subsets only |

The important asymmetry is not 94% versus 16%. It is that the JavaScript
language core is the strongest layer, the Web API layer is deliberately thin, and
the CSS number mixes shipping properties with drafts. A general-purpose parity
target would fight the RAM metric; see §8.

Removed from the previous draft: the "~12% overall parity" line, which had no
source in `PLAN.md`.

---

## 3. HTML

- **142 elements** are defined by the HTML Living Standard.
- The tokenizer/parser is generic: any unknown tag becomes a normal element, so
  parsing coverage is effectively complete.
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
- Declarative shadow DOM, inline `<svg>`, `<script>`/`<style>`/`<link>`/`<meta>`.

---

## 4. CSS

### 4.1 Property names

| Metric | Count |
|---|---:|
| Properties in `@webref/css` | 822 |
| Non-vendor properties | 752 |
| Names in the engine's `KNOWN` table | 141 |
| Additional accepted alias | 1 (`border-top-left-radius`) |
| Raw missing (non-vendor) | ~630 |
| Coverage (non-vendor) | ~16% |
| Coverage against names shipping in two mainstream engines | ~20-22% |

The `KNOWN` table is the source of truth for this count. A name in the table can
still be only partially implemented; a name outside it can still work through a
shorthand. For example, `border-radius` is handled and `border-top-left-radius`
is accepted as an alias, but the other three corner longhands are genuinely
ignored.

### Coverage by category (raw dataset diff)

| Category | Missing | Examples | Decision relevance |
|---|---:|---|---|
| Other / miscellaneous | 217 | `bookmark-label`, `block-step-align`, `corner-*`, `cue` | ignore: drafts, print, speech |
| Text & typography | 61 | `hyphens`, `text-shadow` | high: frequent in real CSS |
| Logical properties & writing modes | 61 | `block-size`, `inline-size`, `margin-trim` | low: low frequency here |
| Animation & transitions | 46 | `animation-*`, `transition-*` | medium only if motion is required |
| Borders & outlines | 35 | corner longhands, `border-style`, `border-image-*` | medium: common in resets |
| Scrolling | 33 | `scroll-behavior`, `scrollbar-*`, `overscroll-behavior` | low: static captures |
| Paged & multi-column | 31 | `break-*`, `column-*` | low |
| Fonts | 23 | `font`, `font-variant-*`, `font-kerning` | medium |
| Grid & alignment | 12 | `grid-row`, `grid-auto-flow`, `place-items` | high |
| Transforms & motion paths | 14 | `perspective`, `offset-*`, `transform-style` | medium |
| Overflow | 14 | `overflow-anchor`, `overflow-clip-margin-*` | low |
| Backgrounds | 13 | `background-clip`, `background-origin`, `background-attachment` | medium |
| Masking & clipping | 13 | `mask-border-*`, `mask-mode`, `mask-composite` | low |
| Containment, containers & anchors | 13 | `contain`, `container-*`, `anchor-*` | low |
| Box, display & positioning | 11 | `appearance`, `all`, `float-offset` | medium |
| Color & theming | 9 | `accent-color`, `caret-color`, `caret-shape` | low-medium |
| Lists | 6 | `list-style-image`, `marker-*` | low |
| Filters & compositing | 5 | `filter`, `backdrop-filter`, `mix-blend-mode` | high for visual parity |
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

- No gradients, no animations or transitions, no `@keyframes`.
- `@media` only understands width, hover, pointer, orientation, and
  `prefers-color-scheme`.
- Grid: no `auto-fill`/`auto-fit`, no `min-content`/`max-content`, `repeat()`
  capped at 64; `place-items`/`place-self` are absent; `justify-items` and
  `justify-self` align grid items inside their area; `align-content`
  distributes rows, but `stretch` packs at the start (auto rows never grow).
- Flex: `wrap-reverse` does not reverse the cross axis.
- Borders: one colour for all sides; dashed/dotted approximated; inner shadows
  dropped; only the top-left corner longhand is aliased.
- `%` `min-height` / `max-height` resolve against width, not container
  height; an auto block still ignores a plain `height` (only `min-height`,
  `max-height`, `aspect-ratio` and replaced boxes pin it).
- `outline` paints a solid ring only (styles collapse, `auto` is off);
  `clip-path` is `inset()` only.
- Interaction pseudo-classes (`:focus`, `:focus-visible`, `:checked`,
  `:active`) drop the whole rule; only `:hover` and `:not(:focus)` survive.
  Focus rings and custom checkbox states are therefore missing.
- `var()` is a flat page-wide map, not a per-element cascade.
- `background-clip` and `filter` are absent.
  `transform-origin` is honored for the translate/scale composite only (no
  3D, and glyph size still does not scale — see the wave 53 notes);
  `overflow-wrap` / `word-break` now break inside words during line layout,
  `text-overflow: ellipsis` marks a clipped line and `max-height` caps a box
  (see the wave 49-52 notes); `hyphens` is not modeled.
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
| Conformance tests | none - 812 self-authored unit tests (816 with `engine-b + woff2`), zero conformance-suite runs |

---

## 8. Turning this into decisions

### 8.1 Denominators that mean something

Surface coverage against a full spec dataset is a proxy that rewards
implementing drafts. Use one of these instead:

1. **Reference-site denominator:** the union of CSS declarations, DOM members,
   and JavaScript APIs actually touched by the six reference pages. This is the
   only denominator that directly measures the success criterion.
2. **Shipping denominator:** features that at least two mainstream engines ship.
   Label single-engine and draft-only features and keep them out of headline
   numbers.

The best signal is neither: instrument the engine to log unresolved property
names and missing DOM/JS members during a reference run, then rank by frequency
and visual impact. That turns this document from a wish list into a work queue.

### 8.2 Priority by reference impact and RAM cost

| Priority | Work | Why | Reference impact | RAM cost | Effort |
|---|---|---|---|---|---|
| P0 | Observed-miss instrumentation for CSS names and DOM/JS member reads | replaces speculation with a ranked list | all six | negligible (dev-only) | short |
| P0 | Shrink JS AST/env/object types (`Stmt` 192 B, `Param` 144 B, `Obj` 88 B, `Closure` 112 B) | directly serves the success metric; wave 44 saved 36 MB on one workload | all | negative (saves) | medium |
| P1 | `form.controlName` shorthand on the form wrapper | some libraries read controls directly off the form | some pages | low | 1 wave |
| P2 | `background-clip`, `filter`/`mix-blend-mode`, gradients, transitions/`@keyframes` | visual fidelity on modern marketing pages | verify against captures | medium | 2-4 waves |
| P3 | `<canvas>` 2D, `<iframe>`, `<video>`/`<audio>` | large new subsystems; only if a target page changes | none of the six | high | weeks-months |
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
   six reference captures is more aligned with this project.
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

## Appendix A - CSS property names accepted (142 + 1 alias)

```text
align-content, align-items, align-self, aspect-ratio
background, background-color, background-image, background-position, background-repeat
background-size, border, border-bottom, border-collapse
border-color, border-left, border-radius, border-right
border-spacing, border-top, border-width, bottom
box-shadow, box-sizing, caption-side, clear, clip
clip-path, color, color-scheme, column-gap
content, cursor, display, fill
fill-opacity, fill-rule, flex, flex-basis
flex-direction, flex-flow, flex-grow, flex-shrink
flex-wrap, float, font-family, font-size
font-style, font-weight, gap, grid-area
grid-column, grid-column-end, grid-column-start, grid-template-areas
grid-template-columns, grid-template-rows, height, inset
justify-content, justify-items, justify-self, left, letter-spacing, line-height
list-style, list-style-position, list-style-type, margin
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
padding-left, padding-right, padding-top, place-content
position, right, row-gap, stroke, stroke-linecap
stroke-linejoin, stroke-opacity, stroke-width, table-layout
text-align, text-decoration, text-decoration-color, text-decoration-line
text-decoration-style, text-decoration-thickness, text-indent, text-overflow
text-transform, text-underline-offset, top, transform
transform-origin, vertical-align, visibility, white-space
width, word-break, word-spacing, word-wrap
z-index
```

`border-top-left-radius` is accepted as an alias of the corner-radius shorthand
but is not listed in `KNOWN`.

## Appendix B - Missing CSS names worth considering

This is a curated shipping-relevant subset, not the raw 630-name dataset dump.
The full list is reproducible with Appendix D.

```text
Layout and alignment:
  place-items, place-self, grid-auto-columns, grid-auto-rows, grid-auto-flow, grid-row,
  grid-row-start, grid-row-end, grid-template, grid-gap

Box, border, outline:
  corner longhands other than top-left, border-style, border-image*,
  box-decoration-break

Text and typography:
  hyphens, text-shadow, text-emphasis, line-clamp, tab-size, quotes,
  direction, unicode-bidi, writing-mode, text-orientation,
  white-space-collapse, text-wrap, font, font-stretch, font-variant,
  font-feature-settings, font-kerning, font-size-adjust, font-optical-sizing

Backgrounds and effects:
  background-clip, background-origin, background-attachment,
  background-blend-mode, filter, backdrop-filter, mix-blend-mode, isolation

Transforms and motion:
  transform-style, perspective, perspective-origin, backface-visibility, rotate, scale, translate, transition,
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

# Engine property table: 142 names
Select-String -Path src\css.rs -Pattern 'const KNOWN' -Context 0,130
```

Dataset versions at the time of measurement: `@webref/css` 8.7.8,
`@webref/elements` 2.9.2, `@webref/idl` 3.86.0, `@mdn/browser-compat-data` 8.1.8.
