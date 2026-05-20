# superdupe

The fastest duplicate file finder for Windows / NTFS.

`superdupe` is a clean-room Rust implementation built around the observation
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

## Building

```pwsh
cargo build --release
```

The release binary is a single `target\release\superdupe.exe`.

## Usage

```pwsh
superdupe scan D:\Media
superdupe scan C:\Users\me\Pictures --min-size 1M --format json --output dups.json
superdupe dedupe dups.json --strategy oldest --action recycle --dry-run
```

See `superdupe --help` for the full CLI.

## Non-goals

* Linux / macOS support
* Filename- or tag-based fuzzy matching
* GUI in v0.1 (planned post-MVP)

## License

MIT. See [LICENSE](LICENSE).
