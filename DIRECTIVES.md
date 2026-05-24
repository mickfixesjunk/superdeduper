# superdeduper — project directives

Canonical priority order for engineering decisions. When directives
conflict, the lower-numbered one wins. CLAUDE.md is a pointer to
this file.

---

## #1 Performance — faster than czkawka

Being measurably faster than czkawka on real workloads is the
single highest-priority product property. If a feature degrades
this, it doesn't ship.

### Scope: user workloads

Directive #1 applies to **user workloads** — the trees a typical
sd user actually runs the tool against:

- `Documents`, `Downloads`, `Dropbox`, OneDrive, iCloud Drive
- `AppData` (Windows) / `Library/Application Support` (macOS) /
  `.config` + `.local/share` (Linux)
- Photo libraries (cameras, phone backups, Lightroom catalogues)
- Downloader / archive trees (torrent landing zones, browser
  download dirs, scratch download-to-keep folders)
- Mixed heterogeneous trees of the above

The 9950X3D + prior-box benchmark anchors for user-workload regimes:

| corpus | sd vs cz | source |
|---|---|---|
| Knight Rider (photo-heavy) | sd 1.95× faster | HANDOVER perf landscape |
| Dropbox (mixed) | sd 1.74× faster | HANDOVER perf landscape |
| large-dups synthetic | sd 1.07× faster | Block V baseline |
| Documents-class | sd faster | testdesign benches |

### Carve-out: OS-system trees (acknowledged-slower territory)

`C:\Windows` and similar OS-system-tree workloads — heavily
hardlink-aliased (WinSxS dominates, ~4× hardlink-alias density
between path-aware and inode-aware dup-counting) — are
**acknowledged-slower territory**. czkawka beats sd on these
trees by 1.3–1.6× depending on hardware:

| measurement | hardware | sd vs cz |
|---|---|---|
| C:\\Windows T0.4 (v3) | prior box | cz 1.46× faster |
| C:\\Windows T0.4 (v4) | prior box | cz 1.57× faster |
| C:\\Windows T0.4 (v5) | 9950X3D   | cz 1.37× faster |

Root cause per r7 analysis: 45% of `C:\Windows` wall-clock is
Tier-1 head-read cost, and Tier-1 is IO/syscall-bound (~4.78
MB/s/thread; identical between river5 and BLAKE3, ruling out
hash-compute). The fix is a Tier-1 IO refactor (mmap reads,
batched directory enumeration, handle pooling) — **deprioritised
and not on the roadmap** because user-workloads aren't gated on
it.

C:\\Windows is still a regression-safety bench (catches accidental
slowdowns) but **no longer a success-criterion gate**. Marketing /
positioning frames as "faster than czkawka on the workloads you
actually run."

**Re-open** the Tier-1 IO refactor only if it becomes the
headline blocker for a feature shipping (e.g. T1.4 archive
content scan turning archive directory enumeration into the new
hot path).

---

## #2 Features — per design's scope agreements

Ship the features design has agreed on. Don't add features
design hasn't agreed on. When in doubt, ask design on
`design-superdeduper.md`.

Specs of record live in `~/sd-bench-local/design/`. Read the
spec before implementing, surface scope ambiguities to design
before writing code.

---

## #3 Everything else

Code quality, tests, docs, dev-loop ergonomics, internal
refactoring. Always desirable, never a reason to delay #1 or #2.

---

## When directives conflict

* #1 beats #2: don't add a feature that makes scans slower on
  user-workloads.
* #2 beats #3: ship features design has agreed on even if the
  code isn't beautiful yet (we can refactor).
* #1 beats #3: a 5% slowdown on user-workloads is never worth a
  cleaner abstraction.

When unsure, post on `design-superdeduper.md` and ask.
