# Project rules

The short version: a from-scratch, pure-Rust browser engine whose only scoreboard
is steady-state RAM against a small set of reference captures. Every rule below
serves that.

## 1. Scope and success

- Pure Rust, from scratch. No vendored browser, layout, CSS, or JavaScript engine
  code.
- Success = the reference pages render like their captures, with zero script
  errors, at the lowest RAM we can reach.
- Reference pages: Wikipedia `/wiki/JavaScript`, MDN homepage, nextjs.org,
  rust-lang.org, Hacker News, google.com homepage; example.com is the shell
  floor. google.com joined in wave 77 with a first baseline (1400px / 28 runs
  / 95.8 MB); its oversized search form and two undiagnosed black shapes are
  known gaps.
- A feature that does not improve one of those pages must justify itself as
  infrastructure (correctness, safety, or measured RAM) before it is built.

## 2. Dependencies

- Approved pure-Rust utilities: `wuff`, `image-webp`, `brotli-decompressor`,
  `url`.
- A new crate needs: pure Rust, no engine or shell logic, a stated RAM and binary
  cost, and a PLAN.md note. Reject anything that embeds a browser or JavaScript
  engine.
- No JIT.

## 3. RAM is the metric

- Bound everything: caches, queues, recursion, page size, script steps,
  microtasks.
- Prefer interning and sharing over per-site allocation (wave 44).
- Report steady-state working set and the stage: `after fetch`, `after parse`,
  `after box tree`, `after layout`.
- When live RSS noise is larger than the effect, use a deterministic synthetic
  bench and say so. Never present noise as a win.
- Do not add general-purpose API surface for parity points; surface costs RAM
  and attack area.

## 4. Reference fidelity

- After a change, run every affected reference page. Record total height, run
  count, working set, and script errors.
- Zero new script errors. A geometry change needs an explanation tied to a
  reference capture.
- No hardcoded site names, URLs, colors, or token strings in engine logic. Site
  names may appear in doc comments and tests.

## 5. Public-surface hygiene

- Never name another browser engine or JavaScript engine project anywhere in the
  tree, commit messages, diffs, comments, docs, or test names. That includes
  reference parsers and CSS stacks named after their parent project.
- Never write a vendor-prefixed CSS property literally. Say "vendor-prefixed
  property" in prose; strip the prefix at the parser boundary.
- Before every commit, scan the staged diff and the message file. The scan list
  is assembled from fragments so this rule file stays inside the rule:

```powershell
# fragments, not names: this keeps the rule file itself clean
$names = @(('chro'+'mium'), ('web'+'kit'), ('ge'+'cko'), ('ser'+'vo'),
           ('bl'+'itz'), ('quick'+'js'))
$prefixes = @(('-'+'web'+'kit'+'-'), ('-'+'moz'+'-'))
$msg = "$env:TEMP\commit.txt"
$hay = ((git diff --cached --no-color) -join "`n")
if (Test-Path $msg) { $hay += "`n" + (Get-Content $msg -Raw) }
foreach ($t in $names + $prefixes) {
    if ($hay -match [regex]::Escape($t)) { throw "forbidden token in commit: $t" }
}
```

## 6. JavaScript

- Engine A (from-scratch tree-walking interpreter) is the default. Engine B is
  feature-gated; both configurations must build and pass.
- New globals and methods get a test that fails without them.
- Keep the documented resource bounds (regex steps, microtasks, page cap, JS
  step budget).

## 7. Workflow

- `ponytail`: smallest complete change. Honest `shortcut:` comments where a
  limitation is deliberate. Tests for non-trivial logic.
- Update PLAN.md every wave: table row plus `### Wave N findings`, then push.
- Test with `cargo test` and `cargo test --features "engine-b woff2"`.
- Never run `cargo fmt` in this repo; it rewrites the whole tree.
- Never synthesize mouse or keyboard input. Verify with unit tests, stderr logs,
  PrintWindow captures, initial-URL launches, and dump modes. Only stop processes
  you started.
- Source edits go through the editor tools or
  `[System.IO.File]::WriteAllBytes` with UTF-8 (no BOM). PowerShell
  `Out-File`/`Add-Content` can re-encode source files.
- Commit messages: write to a file, then `git commit -F <file>`.
- `cargo` exits non-zero in PowerShell even on success; read `test result:` and
  `Finished`, not the exit code.
- Every reply ends with a TLDR.

## 8. Analysis and documentation

- A spec-surface diff is inventory, not a score. GAP-ANALYSIS.md follows the
  rules here.
- Count only what the source does: parser table, match arms, and tests.
  Documentation and READMEs can be stale.
- Use one of two denominators: (a) features used by the reference sites, or
  (b) features shipping in at least two mainstream engines. Label draft-only,
  obsolete, and single-engine features and keep them out of headline numbers.
- Every gap row states: user-visible effect, affected reference page (or none),
  RAM cost, effort, and priority.
- No number without a reproduction command or a stated estimate. Do not cite a
  source that does not contain the number.
- Prefer observed misses from instrumented runs over spec tables when choosing
  the next wave.

## 9. Commit checklist

1. `cargo test` and `cargo test --features "engine-b woff2"` pass.
2. `cargo build` passes.
3. Affected reference pages run; record height/runs/MB/errors.
4. PLAN.md updated.
5. Staged diff and commit message scanned for names and prefixes.
6. Commit and `git push origin master`.
7. Reply with the changes, numbers, gaps/risks, and TLDR.
