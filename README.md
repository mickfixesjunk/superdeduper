# superdeduper

The fastest duplicate file finder for Windows + Linux.

> ⚠️  **ALPHA SOFTWARE — USE AT YOUR OWN RISK.**
>
> superdeduper performs destructive operations on your files (Recycle,
> hardlink replacement, archive move, safe-rename). The defaults are
> reversible (Recycle Bin, safe-rename, archive-with-manifest), but
> bugs in this codebase **can result in permanent data loss**. Before
> running this on important data:
>
> 1. **Back up first.** Test on copies, not originals.
> 2. **Read the action you're about to take.** Each destructive
>    operation requires you to type `DELETE` to confirm by default.
> 3. **Verify the keeper.** The "smart keep" heuristic picks one file
>    per duplicate group based on filename / path / mtime signals — it
>    can be wrong for your specific use case. Click the row before
>    bulk-deleting.
> 4. **Don't run on system folders.** `--allow-system-paths` is
>    deliberately off by default. Don't turn it on unless you know
>    what you're doing.
>
> The authors accept no responsibility for lost data. This is a
> personal project shared as-is. File issues if something breaks,
> but expect rough edges until v1.0.

`superdeduper` is a clean-room Rust implementation built around the observation
that most "fast" cross-platform dedupers leave significant performance on
the table on Windows by avoiding NTFS- and Win32-specific tricks. The
Windows build leans on direct MFT enumeration, LCN-ordered I/O, IOCP,
the USN journal, and format-aware fingerprints. The Linux build uses the
same scanner + hasher with platform-native dedup actions (FICLONE reflink
on btrfs / XFS / Bcachefs / ZFS, XDG Trash for safe recycle, hardlink
detection via `st_ino` + `st_dev`).

> Status: alpha. v0.1.x is feature-active under continuous release. The
> current build (v0.1.8+) ships:
> - Full five-stage scan pipeline with `river5` hashing (16-byte,
>   AES-NI hardware-accelerated)
> - GUI with badge wall, post-scan modal, async rank toast
> - Native Linux build with reflink + XDG Trash support
> - Public leaderboards + achievement system (opt-in)

## Design (one-paragraph version)

1. **Inventory** every file via `FSCTL_ENUM_USN_DATA` (one sequential
   pass over the MFT), with a `FindFirstFileExW` fallback.
2. **Group by size** — discard any size class with fewer than two files.
3. **Resolve physical layout** via `FSCTL_GET_RETRIEVAL_POINTERS` so
   reads can be sorted by starting LCN; hardlinks and ReFS block clones
   are detected and short-circuited here.
4. **Hash progressively** in four tiers (format-aware → 4 KiB head → head/mid/tail → full content via `river5` by default, `BLAKE3` opt-in) so most non-duplicates are eliminated without reading the full file.
5. **Confirm and emit** results, with optional `--paranoid` byte-by-byte verification.

Everything I/O-heavy goes through IOCP queues sorted by LCN with
auto-tuned queue depth (HDD ≈ 32, SSD ≈ 256), and a SQLite cache keyed by
`(volume_guid, file_ref, size, mtime, usn)` makes warm rescans
near-instant via the USN journal.

## Installing a release

### Linux one-liner

```sh
curl -fsSL https://github.com/mickfixesjunk/superdeduper/raw/main/scripts/install.sh | sh
```

Downloads the latest tagged release tarball, verifies its SHA-256
against the release's `SHA256SUMS`, and installs `superdeduper` +
`superdeduper-gui` to `~/.local/bin` (or `/usr/local/bin` with sudo
if `~/.local/bin` isn't on `$PATH`). Override the install location
with `SUPERDEDUPER_INSTALL_DIR=...`. Pin a specific version with
`SUPERDEDUPER_VERSION=v0.2.1`.

### Manual download

Releases are at
[github.com/mickfixesjunk/superdeduper/releases](https://github.com/mickfixesjunk/superdeduper/releases).
Each release ships standalone binaries for Windows + Linux (CLI + GUI),
plus a `SHA256SUMS` manifest. The intent is to publish [GitHub
Sigstore attestations][gh-attest] alongside each release once
`release.yml` is fully green (currently in repair); manual verification
via `SHA256SUMS` is the interim path:

```pwsh
# Windows (PowerShell)
Get-FileHash superdeduper-v0.2.1-windows-x86_64.zip -Algorithm SHA256
# Compare against the line in SHA256SUMS
```

```bash
# Linux
sha256sum -c SHA256SUMS
```

See [SECURITY.md](SECURITY.md) for the full verification flow.

[gh-attest]: https://docs.github.com/en/actions/security-guides/using-artifact-attestations-to-establish-provenance-for-builds

## Building from source

The CLI:

```bash
cargo build --release --locked --features telemetry --bin superdeduper
```

The GUI:

```bash
cargo build --release --locked --features gui,telemetry --bin superdeduper-gui
```

The `telemetry` feature gates the leaderboard / achievement code. Drop
it (`--no-default-features`) for a telemetry-stripped build. Both
builds target your host platform; for cross-builds to Windows from
Linux see the project's CI workflow (uses `cargo-zigbuild`).

`rust-toolchain.toml` pins the toolchain; `--locked` enforces the
checked-in `Cargo.lock`. Together they make local builds bit-for-bit
match the release workflow.

## Usage

### CLI

```bash
# Windows
superdeduper scan D:\Media
superdeduper scan C:\Users\me\Pictures --min-size 1M --format json --output dups.json
superdeduper dedupe dups.json --strategy oldest --action recycle --dry-run

# Linux
superdeduper scan ~/Pictures --min-size 1M --format json --output dups.json
superdeduper dedupe dups.json --strategy oldest --action reflink --dry-run
```

### GUI

Launch `superdeduper-gui` (Windows: `superdeduper-gui.exe`, Linux:
`./superdeduper-gui`). Add folders in the sidebar; click Start scan;
review groups in the central panel; pick a destructive action from
the Go button (Recycle / Hardlink / Reflink / Archive / Safe-rename).

### Achievements + leaderboard (opt-in)

`superdeduper register` to opt in; `superdeduper achievements list` to
see your grants; `superdeduper.io` for the public leaderboard. Default
share preference is "ask me each time" — flip to auto-submit or
never-submit in Settings → Leaderboard.

See `superdeduper --help` for the full CLI.

## Non-goals

* macOS support (not on the roadmap for v0.1.x; revisit at v0.2+)
* Filename- or tag-based fuzzy matching (superdeduper dedupes by content, not by name)
* Cloud / network-only sync (superdeduper reads local filesystems;
  cloud-placeholder files are detected + skipped by default, not hydrated)

## Acknowledgments

Built by Mick and the SuperDeDuper AI dev team using [Giga-Harness](https://github.com/mickfixesjunk/giga-harness) and [Claude](https://claude.com).

## License

MIT. See [LICENSE](LICENSE).
