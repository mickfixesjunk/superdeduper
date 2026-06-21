# format — AGENTS guide

## Purpose
Per-container Tier 0 fingerprint implementations. Each module reads a small,
structurally-significant region of a known file format (ZIP central directory,
JPEG APPn markers, PNG IHDR + text chunks, MP3 ID3 + frame headers, MP4
moov atoms, MKV/EBML Info element, PDF trailer/xref) and folds those bytes
into a BLAKE3 digest. The digest is consumed by the parent
`src/pipeline/hash/format.rs` dispatcher, which prefixes the format tag and
re-hashes with the project-wide `ContentHasher` to produce the Tier 0
fingerprint.

A match between two Tier 0 fingerprints is treated as "very likely
duplicate" by the pipeline and escalated to Tier 3 (full BLAKE3) for final
confirmation; a mismatch rejects the pair without further reads. Parsing
failure must return `Err` so the caller can fall back to byte-sampling — these
modules must never invent matches.

## Files

### `jpeg.rs`
Walks the JPEG marker stream from SOI up to (but not including) SOS.
Folds every APPn (0xFFE0-0xFFEF) and COM payload, plus the SOFn precision
+ width + height, plus EXIF DateTimeOriginal when present, into BLAKE3.
- Public API: `pub fn fingerprint<R: Read + Seek>(r, _size) -> io::Result<Vec<u8>>`
- Calls: parent `read_u16_be`, `read_u32_be` (only via the `_keep_alive`
  stub at line 180), `seek_to`.
- Who calls this: `super::fingerprint` in `format.rs`.
- Key: `_size` parameter is ignored; iteration stops at SOS/EOI.

### `mkv.rs`
Reads the EBML header (DocType + version), then locates the Segment >
Info element and folds SegmentUID, TimecodeScale, Duration, MuxingApp,
WritingApp payloads (each capped at 256 bytes) into BLAKE3.
- Public API: `pub fn fingerprint<R: Read + Seek>(r, size) -> io::Result<Vec<u8>>`
- Internal: `read_vint`, `read_vint_raw`, `read_ebml_header`, `walk_segment`,
  `walk_info`, `position`.
- Who calls this: `super::fingerprint`.
- Key invariant: element IDs returned by `read_vint_raw` preserve the
  leading VINT marker bit so they match the canonical hex constants
  (e.g. 0x1A45DFA3).

### `mp3.rs`
Folds ID3v2 header + capped tag bytes, then first MPEG audio sync header,
frame count (capped 200_000), and last frame header into BLAKE3. Strips
the trailing ID3v1 "TAG" footer (128 bytes) from the audio region.
- Public API: `pub fn fingerprint<R: Read + Seek>(r, size) -> io::Result<Vec<u8>>`
- Internal: `find_sync`, `walk_frames`, `frame_length` (MPEG-1 Layer III
  bit-rate table; conservative 144-byte fallback for other layers).
- Who calls this: `super::fingerprint`.

### `mp4.rs`
Walks ISO BMFF atom tree (max depth 6, max 1024 atoms per level).
Recurses into `moov / trak / mdia / minf / stbl / udta`. Fingerprints
`mvhd` (creation + modification + timescale + duration), every `tkhd`
(creation + modification + track-id + duration), and `stsz`
(version + sample_size + count + streaming sum of table entries).
- Public API: `pub fn fingerprint<R: Read + Seek>(r, size) -> io::Result<Vec<u8>>`
- Constants: `MAX_DEPTH = 6`, `MAX_ATOMS_PER_LEVEL = 1024`.
- Internal: `walk_atoms`, `fingerprint_mvhd`, `fingerprint_tkhd`,
  `fingerprint_stsz`.
- Who calls this: `super::fingerprint`.

### `pdf.rs`
Reads the last 4 KiB of the file, locates the final `%%EOF`, locates
the `startxref` line preceding it, and folds (a) the trailer region and
(b) up to 64 KiB of the xref-table region into BLAKE3.
- Public API: `pub fn fingerprint<R: Read + Seek>(r, size) -> io::Result<Vec<u8>>`
- Internal: `find_last`, `parse_int_after_ws`.
- Who calls this: `super::fingerprint`.
- Constant: `TAIL_WINDOW = 4096`.

### `png.rs`
Walks PNG chunks, folds the 8-byte signature, IHDR payload, the first
IDAT chunk's `(length, type, first-16-bytes)`, and every text-chunk
payload (`tEXt`, `iTXt`, `zTXt`). Stops at IEND. Each chunk payload is
capped at 16 KiB (`payload_len_limit`).
- Public API: `pub fn fingerprint<R: Read + Seek>(r, _size) -> io::Result<Vec<u8>>`
- Key invariant: CRC is consumed but not verified.

### `zip.rs`
Locates EOCD by scanning the last `22 + 0xFFFF + 64` bytes, parses
central-directory entries, and folds `(filename, uncompressed_size, crc32)`
tuples — *sorted by filename* — into BLAKE3. Returns `Err` for ZIP64
archives (cd_off or cd_size == 0xFFFFFFFF).
- Public API: `pub fn fingerprint<R: Read + Seek>(r, size) -> io::Result<Vec<u8>>`
- Constants: `EOCD_SEARCH`, `EOCD_SIG`, `CD_SIG`, `_Z64_LOCATOR_SIG` (unused).
- Internal: `find_signature_back`.
- Who calls this: `super::fingerprint` (extensions `zip|jar|epub|docx|xlsx|pptx|odt|ods|odp`).

## Invariants / Gotchas
- **Byte-exactness vs goldens**: every `hasher.update(...)` sequence is
  load-bearing. A digest change at this layer changes user-visible Tier 0
  fingerprints and could split previously-matched dup groups. Any
  refactor that reorders or renames the literal byte tags (`"JPEG|"`,
  `"MKV|"`, `"MP3|"`, `"MP4|"`, `"PDF|"`, `"PNG|"`, `"ZIP|"`,
  `"IHDR|"`, `"IDAT0|"`, `"mvhd|"`, `"tkhd|"`, `"stsz|"`, `"sum|"`,
  `"DTO|"`, `"SOF|"`, `"EBML|"`, `"Info|"`, `"AUDIO0|"`, `"AUDIOZ|"`,
  `"COUNT|"`, `"XREF|"`, `"BOX|"`) MUST be done in lock-step with a
  Tier 0 schema version bump in the parent module.
- **ZIP order-independence**: the CD entry sort in `zip.rs` line 80 is
  required so archives with members written in different physical order
  but identical logical contents still match. Tested by
  `entry_order_does_not_matter`.
- **No false positives**: parsing failure must return `Err`; the parent
  converts `Err` to `None` so the file falls back to byte-sampling tiers.
  Never substitute a placeholder digest.
- **Caps are byte-stable**: payload caps (256 bytes for MKV Info fields,
  16 KiB for PNG chunks, 64 KiB for MP3 ID3, 64 KiB for PDF xref, 4 KiB
  for PDF tail, 200_000 frame count, 1024 atoms/level, depth 6 for MP4)
  are all part of the digest definition — changing any of them is a
  Tier 0 schema break.
- **JPEG `_size` and PNG `_size` parameters are intentionally unused**;
  iteration is bounded by the format's own end markers.
- **MP4 stsz table sum** is `wrapping_add` u64 — overflow is silently
  wrapped and that is the on-disk fingerprint contract.

## Dependencies
- INCOMING: `src/pipeline/hash/format.rs::fingerprint` is the sole caller
  in production. Tests inside each module also call directly.
- OUTGOING:
  - `blake3` crate (every module).
  - Parent helpers `read_u16_be`, `read_u32_be`, `seek_to` (used by
    `jpeg.rs`).
  - `std::io::{Read, Seek, SeekFrom}` everywhere.

## Refactor Hints
- **`jpeg.rs::_keep_alive` (lines 179-182)** is a dead stub kept solely
  to suppress an unused-function warning on `read_u32_be`. The parent
  already marks `read_u32_be` with `#[allow(dead_code)]` (format.rs:96),
  so this stub is redundant and can be deleted. Verify by removing and
  running `cargo check` on all targets. (Severity: info / dead-code.)
- **`zip.rs::_Z64_LOCATOR_SIG` (line 22)** is unused — the comment says
  it's "not parsed for fingerprinting." It exists only as documentation;
  fine to leave, but candidate for removal if the file is being pruned.
- **MKV `walk_info` (line 81)**: the `already < payload_end` branch at
  lines 105-108 is the only place the read cursor could be left mid-
  element if the payload was exactly capped at 256 bytes. The current
  logic re-seeks to `payload_end` always when truncated; if not
  truncated the loop's next `read_ebml_header` picks up at the right
  spot via implicit position. Subtle; worth a comment.
- **Common helper opportunity**: every module redefines an `io_err(&str)
  -> std::io::Error` helper. Could be lifted to the parent
  `format.rs` (already has `pub(crate)` helpers there) for a small
  win. (Severity: info.)
- The `walk_frames` MP3 resync loop seeks to `off` then reads a scratch
  buffer just to find a sync byte; it could reuse the already-read 4
  bytes in `hdr` plus a `Read::take`. Cosmetic.

## Wire Surfaces
None directly. These modules contribute to the *Tier 0 fingerprint
schema*, which is an on-disk-cached value in the pipeline's hash cache
DB. The byte tags listed under "Invariants / Gotchas" plus the cap
constants together define that schema; refactor coordination required
with the cache / pipeline tier-version logic in the parent crate.

## Non-source artifacts
None. The directory contains only `.rs` files.
