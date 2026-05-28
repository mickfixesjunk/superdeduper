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

### S14 — Comments must not include factual claims about other parts of the system

> Cross-references rot fastest. A comment that says "matches the
> CLI default" or "per #87" or "see the X module's behavior" is
> correct on the day it's written; six months later, when the
> referenced thing changes, the comment becomes a lie that
> misleads future readers. Comments state intent ("why this
> exists") and local invariants ("this branch handles X"); they
> do not summarize the state of other code.

**Bad** (the class quality flagged five times in eight pre-push
reviews over the 2026-05-27 burn cycle):

```rust
// matches the CLI default (#87, phash-tuned for photo corpora)
let threshold = 10;
```

That comment was correct when written. After #127 reverted the
CLI default to dhash+τ=5, the comment became misleading. Future
reader trusts the claim; bug ships.

```rust
/// GetSystemDirectoryW returns its full path; we extract the
/// drive letter and probe THAT volume's persistent flags.
fn detect() -> bool {
    let path: Vec<u16> = "\\\\?\\C:\0".encode_utf16().collect();
    // ... hardcoded C: ...
}
```

The comment promised one implementation; the code did another.
Reviewer caught it; would have shipped silently if the comment
hadn't been read against the code.

```rust
// integer-arithmetic version avoids floats
let scale = ((n / 100) as f64).log10().floor() as u32;
```

Same shape — comment claimed integer arithmetic, code used floats.
Production correct on x86/ARM, but the claim was a future-proofing
trap waiting to fire on the first platform whose libm rounds
log10(100.0) to 1.9999...

**Good**: comments describe intent + local invariants only.

```rust
// Floor at 3 so τ never collapses to zero on huge corpora where
// dHash's bit-precision tops out at 5.
let scale = (n / 100).checked_ilog10().unwrap_or(0);
default_tau.saturating_sub(scale).max(3)
```

The comment explains WHY (the floor). It doesn't make any claim
about what the code in `(n/100).checked_ilog10()` does — the code
reads itself.

**The discipline:** before posting any commit for review, **scan
every touched file's comments for factual claims about other
code, other modules, defaults, or invariants. Either verify the
claim against current code in the same commit, or delete the
claim.** Quality has caught this class on five of the last eight
pre-push reviews; the cost of a one-minute self-scan beats the
cost of every NIT cycle.

## Where these came from

The numbering (S11-S13) reflects the principal-engineer code review
on 2026-05-27. S1-S10 were issue findings in that same review,
tracked as individual GitHub issues (#131 et al.) — these three
(S11-S13) were the standing-policy additions that needed a doc home.

S14 was added 2026-05-27 after the burn-cycle pre-push reviews
surfaced comment-vs-code drift in five of eight reviews (#130
GetSystemDirectoryW; E3 NIT 1 integer-arithmetic; E3 NIT 2 stale
phash; #74 NIT path_display carve-out; #57 NIT SERIAL-mutex
claim). Quality flagged the cadence; design routed the
standing-principle slot.

## Regenerating the wire schema

`schema/submit.schema.json` is the canonical leaderboard wire
contract. It is generated FROM the Rust structs (RunShape,
ResultSummary, HardwareFingerprint) via schemars — never hand-edit
it. The `submission_schema_matches_committed` test is a drift gate:
it derives the schema fresh and string-compares against the
committed file, failing the build if they diverge.

To regenerate after a deliberate struct change:

```
SD_UPDATE_SCHEMA=1 cargo test --features telemetry submission_schema_matches_committed
```

Then review the diff and commit the regenerated file alongside the
struct change, so the contract change is visible in one commit.

**The gate also fires on schemars version bumps.** A `cargo update`
that changes the schemars version (even a patch within 0.8) can
alter the emitted formatting — key ordering, whitespace, `format`
annotations — and fail the test even with no struct change. This is
intentional: schemars' output IS the wire contract, so a toolchain
bump that reshapes the emitted schema is a contract change that
deserves a deliberate regen + diff review. If a routine dep update
trips this test, run the regen command above and review what moved.
