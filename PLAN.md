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
  images with LRU decode cap
- [x] **Phase 1c (Track A)** — box tree + flex/grid layout: ~~flat atom flow~~ **DONE:
  `BoxNode` tree (block/flex/grid groups + text leaves), `BoxStyle` cascades from CSS
  (display/gap/justify/align/width/margin-auto/text-align), block flow with legacy
  paragraph-gap semantics, flex row (max-content sizing, proportional shrink,
  justify, cross-axis align), flex column (row-gap), grid (fr/px/pct tracks,
  repeat()/minmax(), row-major cells), text-align per line (58 tests; nextjs.org
  hero centered + feature cards stacked, RSS 19.2 → 16.9 MB)**. Known gaps:
  no flex-wrap, no per-side margin/padding, no min-width, no position:\*,
  no flex-grow/basis, no line-height, no per-box background-color,
  stretch doesn't reflow children
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
