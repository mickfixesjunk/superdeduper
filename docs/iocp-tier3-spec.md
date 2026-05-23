# Block O+: IOCP integration for Tier 3 hashing

> **Status:** spec + implementation plan.
> Engine work splits into two chunks. Block V's baseline bench
> (large-dups-r1) sets the win target.

---

## The opportunity

`src/pipeline/iocp.rs` has the scaffolding (Scheduler trait,
ReadRequest, ReadCompletion, LCN-ordered submission queue), but
`WindowsScheduler::run_to_completion` is currently a STUB — it routes
through the buffered backend (synchronous reads via `std::fs::File`).
That's why Block O (SEQUENTIAL_SCAN) only moved Tier 3 throughput
+10%: one rayon worker still serializes its file reads even with
prefetch.

With IOCP done, each worker can have many reads in flight against the
same physical drive. The OS drains them in LCN order on HDDs, in any
order on SSDs. The disk pipeline never stalls waiting for a single
file's CreateFile + first read to complete.

Expected impact (per the dropbox-r1 + dropbox-r2 wall analysis):
* On pure-large-file workloads (synthetic corpora; Block V): 30-50%
  Tier 3 wall reduction. Disk seq read ceiling ~5 GB/s; we currently
  hit ~2 GB/s wall on Dropbox = 40% of ceiling.
* On heterogeneous workloads (Dropbox 198 GB class): 5-10%. Tier 1
  saturation already caps total wall; reducing Tier 3 by 50% only
  helps the fraction of wall that Tier 3 actually owns.

## Two chunks

### Chunk 1: complete `WindowsScheduler::run_to_completion`

The actual IOCP submit/wait loop. Touches:

* `src/pipeline/iocp.rs::win::WindowsScheduler::run_to_completion`
* New: file-handle pool wired to the completion port
* New: VirtualAlloc-based sector-aligned read buffer pool

Pseudocode:

```rust
pub fn run_to_completion(
    &self,
    mut on_complete: impl FnMut(ReadCompletion),
) -> Result<()> {
    // 1. Drain the pending LCN-ordered queue, opening file handles
    //    with FILE_FLAG_OVERLAPPED | FILE_FLAG_NO_BUFFERING and
    //    associating them with `self.iocp` via `associate(...)`.
    //
    // 2. Issue up to `self.queue_depth` initial ReadFile calls. Each
    //    one immediately returns ERROR_IO_PENDING because of
    //    OVERLAPPED. Track the OVERLAPPED struct, the buffer, and
    //    the originating ReadRequest in a per-key registry.
    //
    // 3. Loop: GetQueuedCompletionStatus(self.iocp, ...) blocks
    //    until a read finishes. For each completion:
    //    - Look up the registry entry by the completion key.
    //    - Build a ReadCompletion { request, bytes, latency_us }.
    //    - Call on_complete(...).
    //    - If there's another request in self.pending, issue it.
    //
    // 4. Continue until self.pending is empty AND no in-flight reads
    //    remain.
}
```

Subtleties:
* **Sector alignment.** FILE_FLAG_NO_BUFFERING requires offset, length,
  and buffer pointer all aligned to the volume sector size. Use the
  existing `buffered::align_up` for size; cache the per-volume sector
  size (4096 on most NVMe, 512 on legacy).
* **Buffer pool.** Each in-flight read needs its own buffer
  (queue_depth × buffer_size ≤ a few MB per worker). Pool the buffers
  rather than allocating per-read. `VirtualAlloc(LARGE_PAGES)` if
  available; otherwise plain `VirtualAlloc`.
* **OVERLAPPED lifetime.** Each in-flight read needs a stable pointer
  to its OVERLAPPED struct (passed to ReadFile and returned by
  GetQueuedCompletionStatus). Easiest: Box<OVERLAPPED> per read,
  stored in a HashMap<OVERLAPPED_ptr, RegistryEntry> with raw pointer
  as key. Drop the box when the completion fires.
* **Error handling.** GetQueuedCompletionStatus returns FALSE on read
  error; the per-key registry tells us which file failed. Surface
  via ReadCompletion with `bytes = empty + error field`. Tier 3
  callers treat empty-completion as "hash failed for this file."

### Chunk 2: wire it into Tier 3

Touches `src/pipeline/hash.rs`:

* New helper `tier3_via_iocp(group: &[&LaidOutFile], algo, cancel,
  counters) -> HashMap<PathBuf, Result<Vec<u8>>>` that:
  1. Builds N ReadRequests (one per file, file_offset=0,
     length=file size rounded up to sector align)
  2. Submits all to the scheduler
  3. run_to_completion: for each ReadCompletion, hashes the bytes
     and stores the digest keyed by path
  4. Returns the digest map
* `run_group`'s Stream A and Stream B tier 3 calls switch to use the
  batched IOCP helper for large files (≥ TIER3_ONESHOT_THRESHOLD).
  Small files keep their existing direct-read path (oneshot in-memory).
* Per-scan IOCP scheduler — share across all run_group calls so the
  queue depth amortizes across workers. New field on the engine state.

## Risks + test mitigations

| risk | mitigation |
|---|---|
| Sector-alignment regression (corrupts hashes) | Equivalence test: compare hash output of `tier3_via_iocp(file)` against `tier3_hash_cancellable(file)` for ≥20 fixture files of varying sizes including non-sector-multiples |
| Completion-port deadlock if pending isn't drained on cancel | Cancel path: drain remaining completions before returning |
| Buffer pool leak on cancel/panic | Drop guard on the buffer pool entries |
| Non-NTFS / non-OVERLAPPED filesystems | Fall back to `BufferedScheduler` when CreateIoCompletionPort fails |
| Per-rayon-worker scheduler proliferation | Share one scheduler across all workers; queue per-volume |
| Cross-platform | The iocp module is already cfg-gated. Non-Windows uses BufferedScheduler — already wired. No new cross-platform concerns |

## Bench validation (Block V baseline)

`large-dups-r1-baseline` from Block V sets the IOCP-off number. After
Block O+ lands, run `large-dups-r2-iocp` against the same corpus and
look for:

* Wall ≤ 70% of r1 (30%+ improvement on pure-large-file)
* Tier 3 throughput / disk seq read ratio ≥ 70% (was 35% on dropbox-r1)
* dup groups + reclaimable byte-identical (output stability)
* IOCP-active stderr log line confirms scheduler engaged

## Out of scope for first iteration

* HDD-specific tuning (LCN-ordered submission is already there; per-
  drive queue depth heuristic comes later)
* Direct I/O (FILE_FLAG_NO_BUFFERING) — first impl uses default
  buffered overlap, which is simpler. Direct I/O is a follow-up
  optimization once we've measured what default-overlap delivers.
* Tier 1 / Tier 2 IOCP — those are syscall-bound, not IO-bound. Block
  N's batched dir enum already addresses Tier 1. No IOCP win expected.

## Sequencing

1. **Now**: wait for Block V's baseline number (large-dups-r1).
2. Implement Chunk 1 (scheduler completion). Bench-validate with a
   unit test against a known-content fixture.
3. Implement Chunk 2 (Tier 3 wire-in). Bench-validate via
   large-dups-r2-iocp against the Block V baseline.
4. If Block V's baseline number is below the IOCP-IO-bound regime
   (e.g., if even synthetic large-file-dup workloads aren't disk
   limited), reconsider scope: maybe SEQUENTIAL_SCAN alone was
   enough and IOCP is for HDD users only.
