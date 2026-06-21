# .github/workflows AGENTS.md

## Purpose

GitHub Actions for the superdeduper engine. Two workflows:

- `ci.yml` — gate every push to `main` + every PR + manual dispatch.
  Format check, feature-flag drift gate, clippy across four feature
  combos, Linux + Windows tests, Windows CI artifact build (uploaded to
  the workflow run, not a release).
- `release.yml` — runs on `v*` tag push (or manual dispatch with a tag
  input). Cross-platform build matrix (Windows MSVC, Linux musl, macOS
  x86_64 + aarch64, FreeBSD 14.1 via VM), Sigstore build-provenance
  attestation, draft release upload, canonical SHA256SUMS aggregation,
  auto-publish + flip the "Latest" pointer.

## Files

### `ci.yml`

Jobs:

- `fmt` — `cargo fmt --all -- --check`.
- `feature-flag-drift` — runs `scripts/check-feature-flag-consistency.sh`
  to diff feature flags between `scripts/cross-build-drop.sh` and
  `release.yml`. Spec ref: #69 (catches v0.2.4-class drift where the
  release binary shipped without `telemetry`/`similar-*`).
- `clippy` — four invocations: bare, `--features gui`, `gui similar-images
  similar-audio`, and `gui telemetry similar-images similar-audio`. Each
  with `-D warnings`. `audio` deliberately omitted because alsa-sys needs
  libasound2-dev (Linux runner doesn't have it; Windows test job covers
  `audio`).
- `test-linux` — `cargo test --workspace --locked` then again with
  `--features gui` (the resumability tests live under that feature
  gate).
- `test-windows` — adds Defender exclusions for `$GITHUB_WORKSPACE`,
  `$TEMP`, `$RUNNER_TEMP`; runs `cargo test --lib --bins
  --test-threads=1` and `cargo test --test smoke --test-threads=1`
  (skips the `properties` proptest binary on Windows — Linux covers
  algorithm correctness). Builds release binaries with `gui audio`,
  computes SHA256SUMS, uploads `superdeduper-windows-<sha>` artifact
  on push-to-main / dispatch with 14-day retention.

### `release.yml`

Workflow-level `permissions: contents: write, id-token: write,
attestations: write` for Sigstore attestation. `RUSTFLAGS: -D warnings`
deliberately NOT set workflow-wide (deny-warnings would have turned
every tag red across the egui 0.28→0.32 deprecation backlog).

Jobs:

- `build` (Windows MSVC matrix) — only `x86_64-pc-windows-msvc`.
  `aarch64-pc-windows-msvc` removed (river5 AES-NI C source uses x86
  intrinsics; tracked as the same blocker that `aarch64-apple-darwin`
  was, but Windows ARM64 still pending). Builds with
  `RUSTFLAGS=-C target-feature=+crt-static`, feature sets `telemetry
  similar-images similar-audio` (CLI) and `gui telemetry audio
  similar-images similar-audio` (GUI). Optional Authenticode signing
  via `CODE_SIGN_PFX_BASE64` + `PFX_PASSWORD` secrets. Computes
  inside-archive SHA256SUMS. Zips, attests build-provenance (skipped on
  user-owned private repos; `continue-on-error: true`), uploads to
  draft release.
- `build-linux` — `x86_64-unknown-linux-musl`, fully static. Installs
  `musl-tools libasound2-dev pkg-config`. `audio` feature OFF (rodio →
  alsa-sys doesn't static-link clean against musl in practice). Strips
  symbols. Hard fail if `objdump -T` still references `GLIBC_` symbols.
- `build-macos` — matrix `x86_64-apple-darwin` + `aarch64-apple-darwin`.
  Re-added v0.2.12 after river5 35533ce shipped arch-conditional AES-NI
  gating. `audio` OFF. Unsigned (Gatekeeper "unidentified developer"
  warning; signing+notarisation deferred per platforms-roadmap.md §L1).
- `build-freebsd` — FreeBSD 14.1 via `vmactions/freebsd-vm@v1`. Native
  compile inside the VM (zig 0.13 ships no FreeBSD libc headers, so the
  zigbuild path is structurally blocked). `audio` OFF.
- `checksums` — finalize. `needs: [build, build-linux, build-macos,
  build-freebsd]`. Downloads zips + tarballs from the draft release via
  `gh release download`, computes canonical release-root SHA256SUMS,
  re-uploads, then `gh release edit --draft=false --latest` to flip
  publish + Latest pointer. Per Phase 3 v0.3.22 (2026-06-01) — replaces
  the manual `--draft=false --latest` step engine agent missed across
  v0.3.16-v0.3.20.

## Invariants

- **Feature-flag floor (CI gate).** The `feature-flag-drift` job MUST
  pass. The four clippy invocations in `ci.yml` MUST mirror the feature
  combinations the release builds in `release.yml`. Adding a new
  feature requires: (1) appending it to the clippy feature matrix in
  `ci.yml`, (2) adding it to the relevant release-job `--features`
  string, (3) adding it to the tuple list in
  `scripts/check-feature-flag-consistency.sh`.
- **`--test-threads=1` on Windows.** Property-test binary skipped +
  unit/smoke serialized. Defender exclusions are belt-and-braces, not a
  substitute. Removing either makes the job hang at 20+ minutes.
- **`audio` only on Windows builds** (CI test job + release Windows
  job). Linux/macOS/FreeBSD release legs deliberately omit it. CLI on
  Windows also omits `audio` (only the Windows GUI build enables it).
- **`+crt-static` only on Windows MSVC.** Setting it workspace-wide
  would break Linux musl. If a new Windows target (gnullvm, aarch64)
  comes back, the env-var must be scoped at the step level.
- **`musl-static` hard gate.** `build-linux` exits 1 if `objdump -T`
  finds any `GLIBC_` symbol in `superdeduper-gui`. Adding a non-static
  C dep silently would otherwise ship a binary that breaks on older
  Linux systems.
- **`attest-build-provenance` is guarded.** The
  `repository.visibility != 'private' || owner.type == 'Organization'`
  conditional is load-bearing — GitHub returns "Feature not available
  for user-owned private repositories." Removing the guard surfaces a
  red square on every private-repo release. `continue-on-error: true`
  also load-bearing as belt-and-braces.
- **Two artifact-integrity layers.** Inside-archive SHA256SUMS (added
  by each build job) is for post-extract verification. Release-root
  SHA256SUMS (added by the `checksums` job) is for download-integrity
  verification. Per #11 the release-root file is the canonical source
  of truth; do NOT collapse the two.
- **Draft -> Latest flip auto-runs.** Per Mick directive (see
  [[feedback_github_release_hold_by_default]] revoked 2026-05-30),
  engine releases auto-promote. The `gh release edit --draft=false
  --latest` in the `checksums` job is what realizes that.
- **`VERSION_TAG` resolution.** `${{ github.event.inputs.tag ||
  github.ref_name }}`. On tag push, `ref_name` is the tag; on manual
  dispatch, the input form supplies it. Every per-platform job env +
  the `checksums` env independently re-resolves this; if you add a new
  build job, copy the same env clause.

## Dependencies

INCOMING:

- `scripts/check-feature-flag-consistency.sh` — invoked by the
  `feature-flag-drift` job.
- `scripts/cross-build-drop.sh` — the OTHER half of the
  feature-flag-drift diff target. Lives outside CI but its feature
  strings are diffed against `release.yml`.

OUTGOING (third-party actions):

- `actions/checkout@v4`
- `dtolnay/rust-toolchain@stable`
- `Swatinem/rust-cache@v2`
- `actions/upload-artifact@v4` (ci.yml only)
- `actions/attest-build-provenance@v1` (release.yml)
- `softprops/action-gh-release@v2` (release.yml)
- `vmactions/freebsd-vm@v1` (release.yml `build-freebsd`)

Secrets consumed:

- `secrets.CODE_SIGN_PFX_BASE64` / `secrets.PFX_PASSWORD` — optional
  Windows Authenticode signing (release.yml). Skipped automatically
  when unset (forks, PRs).
- `secrets.GITHUB_TOKEN` — `checksums` job for `gh release download` +
  `gh release edit`.

Cargo feature names referenced (must stay in sync with workspace
Cargo.toml `[features]`):

- `gui`, `audio`, `telemetry`, `similar-images`, `similar-audio`.

## Refactor Hints

- **TESTING.md §6 is stale.** Cites a `windows-latest NTFS (VHD)
  admin`, `ReFS (VHD) admin`, `NTFS (host) non-admin` matrix, plus a
  separate `perf.yml` nightly workflow and a `bench-vs-fclones.yml`
  workflow. Neither perf.yml nor bench-vs-fclones.yml exist in this
  directory; the actual Windows test job is a single non-admin run
  without VHD/ReFS coverage. TESTING.md also claims `clippy::pedantic`
  with an opt-out allow-list — ci.yml runs plain `cargo clippy -- -D
  warnings`, not pedantic. See FINDINGS below.
- **`feature-flag-drift` doesn't cover macOS or FreeBSD.** Per
  `scripts/AGENTS.md` Refactor Hints, the consistency script's tuple
  list is `(windows,cli) (windows,gui) (linux,cli) (linux,gui)`. The
  `release.yml` now has `build-macos` + `build-freebsd` legs with
  their own feature strings — drift in those legs is invisible to the
  gate.
- **Comment line numbers in `scripts/AGENTS.md` reference
  release.yml.** Says `build-macos:` is at "lines 267-319"; current
  file has `build-macos:` at line 267 but it spans 267-363, and the
  `build-freebsd` job starts at 365. Cite-by-anchor (job name) rather
  than line number would survive future edits.
- **`ci.yml` artifact upload uses `${{ github.sha }}`.** If a refactor
  ever switches the trigger to tag-push, swap to `github.ref_name` for
  human-recognizable artifact names.
- **`build-freebsd` runs no tests.** The VM is "capable of running
  `cargo test` if we ever want native FreeBSD tests in the v0.2.x
  cycle" (comment line 379-380). Currently build-only — opportunity
  for a smoke step.
- **Header comment `ci.yml` lines 11-16 mentions removed
  `RUSTFLAGS: -D warnings` env.** The env block now only contains
  `CARGO_TERM_COLOR: always`. Comment is accurate-as-history but could
  be shortened to one line.
- **`release.yml` env comment lines 25-31** is similarly retrospective
  ("had been failing since v0.1.5"). Could be condensed once the
  deprecation backlog is fully cleaned.
- **`Authenticode sign` step gates on `env.CODE_SIGN_PFX_BASE64 != ''`
  but `env` for that step is set further down inside the same step
  block.** The conditional actually evaluates against secrets indirection
  — this works because the step's `env:` block resolves
  `${{ secrets.CODE_SIGN_PFX_BASE64 }}` at expression time, but the
  shape is subtle. Worth a one-line comment.

## Wire Surfaces

External-facing surfaces:

- Release artifacts:
  - `superdeduper-<VERSION_TAG>-windows-x86_64.zip`
  - `superdeduper-<VERSION_TAG>-linux-x86_64.tar.gz`
  - `superdeduper-<VERSION_TAG>-macos-x86_64.tar.gz`
  - `superdeduper-<VERSION_TAG>-macos-aarch64.tar.gz`
  - `superdeduper-<VERSION_TAG>-freebsd-x86_64.tar.gz`
  - `SHA256SUMS` at release root (canonical integrity manifest;
    `scripts/install.sh` consumes this).
  - Inside each archive: `superdeduper(.exe)`, `superdeduper-gui(.exe)`,
    `LICENSE`, `README.md`, `SHA256SUMS`.
- Sigstore attestations: published on every release leg that runs on a
  public/org-owned repo. Verifiable via `gh attestation verify`.
- CI workflow artifact: `superdeduper-windows-<sha>` (14-day
  retention).
- Triggers: `push to main` (ci), `pull_request` (ci), `push tag v*`
  (release), `workflow_dispatch` (both; release requires a `tag`
  input).
