# superdeduper

The fastest duplicate file finder for Windows / NTFS.

`superdeduper` is a clean-room Rust implementation built around the observation
that most "fast" cross-platform dedupers leave significant performance on
the table on Windows by avoiding NTFS- and Win32-specific tricks. By
targeting Windows exclusively we can lean on direct MFT enumeration,
LCN-ordered I/O, IOCP, the USN journal, and format-aware fingerprints to
beat general-purpose tools — especially on HDDs and large media
libraries.

> Status: pre-alpha. v0.1 is under active development. The current build
> contains the CLI surface, the five-stage pipeline scaffolding, and a
> correct (if not yet maximally fast) end-to-end scanner.

## Design (one-paragraph version)

1. **Inventory** every file via `FSCTL_ENUM_USN_DATA` (one sequential
   pass over the MFT), with a `FindFirstFileExW` fallback.
2. **Group by size** — discard any size class with fewer than two files.
3. **Resolve physical layout** via `FSCTL_GET_RETRIEVAL_POINTERS` so
   reads can be sorted by starting LCN; hardlinks and ReFS block clones
   are detected and short-circuited here.
4. **Hash progressively** in four tiers (format-aware → 4 KiB head → head/mid/tail → full BLAKE3) so most non-duplicates are eliminated without reading the full file.
5. **Confirm and emit** results, with optional `--paranoid` byte-by-byte verification.

Everything I/O-heavy goes through IOCP queues sorted by LCN with
auto-tuned queue depth (HDD ≈ 32, SSD ≈ 256), and a SQLite cache keyed by
`(volume_guid, file_ref, size, mtime, usn)` makes warm rescans
near-instant via the USN journal.

## Installing a release

Releases are at
[github.com/mdreeling/superdeduper/releases](https://github.com/mdreeling/superdeduper/releases).
Each release ships a per-architecture zip with `superdeduper.exe`,
`superdeduper-gui.exe`, the LICENSE, and a `SHA256SUMS` manifest. All
artifacts are reproducibly built in public CI and signed twice —
once via [GitHub Sigstore attestations][gh-attest] (always) and once
via Authenticode (when a code-signing cert is configured).

**Always verify a download before running it.** Step-by-step
instructions live in [SECURITY.md](SECURITY.md); the short version is:

```pwsh
gh attestation verify superdeduper-x86_64-windows.zip --repo mdreeling/superdeduper
```

If you see anything other than `verification succeeded`, do not run
the binary — it didn't come from this repo's `release.yml`.

[gh-attest]: https://docs.github.com/en/actions/security-guides/using-artifact-attestations-to-establish-provenance-for-builds

## Building from source

```pwsh
cargo build --release --locked
```

The release binary is a single `target\release\superdeduper.exe`. The
optional GUI:

```pwsh
cargo build --release --locked --features gui --bin superdeduper-gui
```

`rust-toolchain.toml` pins the toolchain; `--locked` enforces the
checked-in `Cargo.lock`. Together they make local builds bit-for-bit
match the release workflow.

## Usage

```pwsh
superdeduper scan D:\Media
superdeduper scan C:\Users\me\Pictures --min-size 1M --format json --output dups.json
superdeduper dedupe dups.json --strategy oldest --action recycle --dry-run
```

See `superdeduper --help` for the full CLI.

## Non-goals

* Linux / macOS support
* Filename- or tag-based fuzzy matching
* GUI in v0.1 (planned post-MVP)

## License

MIT. See [LICENSE](LICENSE).
