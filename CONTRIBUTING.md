# Contributing — engineering discipline

This document captures the project's standing engineering principles
beyond what `README.md`, `TESTING.md`, and `SECURITY.md` cover.

These principles aren't aspirations. They're the corrective output
of incidents the project has actually hit — each one is here because
its absence produced a bug, audit finding, or silent failure that
shipped to users.

## Principles

### S11 — TODOs are intent-only

> TODOs in code state **intent only** — no cross-references to defaults
> or state in other files. Such cross-refs rot fastest.

**Bad** (the F36/F37 anti-pattern that produced #127):

```rust
let threshold = 10; // matches the CLI default (#87, phash-tuned)
```

That comment was correct when written and stale within a release —
the CLI default was reverted in v0.2.13 but the GUI comment kept
claiming "matches the CLI default." Future readers trust the
comment; the bug ships.

**Good**: `TODO: pull from Settings → image-threshold once that
control lands.` Names the work to be done, doesn't pin to drifting
state.

### S12 — Length is an extraction signal

> New functions / structs over 200 LOC are **extraction candidates by
> default**. `#[allow(clippy::too_many_arguments)]` is a config-struct
> hint, not a real suppression.

A long function with 8 arguments isn't a function; it's an
unstructured config object hiding behind a callable. The lint exists
for a reason: when reaching for the allow, reach for a struct instead.

Some legitimately-long functions exist (top-level event loops, render
loops). Document the reason inline when keeping length intentional.

### S13 — Check the dep tree before reinventing

> Check the dep tree before writing a bespoke utility. `url`,
> `urlencoding`, `base64`, `chrono`, `hex` etc. are already in
> `Cargo.lock` — don't reinvent.

The principal-engineer quality review caught ~150 LOC of
hand-rolled url-decode + hex + base64url + ISO-8601-now in
`oauth.rs`, all reinventing crates already pulled in transitively.
Reinventing widens the audit surface (your code vs. an audited
crate), risks subtle correctness bugs (encoding edge cases), and
inflates the diff size on every touch.

Default to: search `Cargo.lock` for an existing dep before writing
the utility. Add a direct dep if needed.

## Where these came from

The numbering (S11-S13) reflects the principal-engineer code review
on 2026-05-27. S1-S10 were issue findings in that same review,
tracked as individual GitHub issues (#131 et al.) — these three
(S11-S13) were the standing-policy additions that needed a doc home.
