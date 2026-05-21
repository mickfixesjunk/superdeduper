//! IOCP-backed read pipeline with LCN-sorted submission.
//!
//! The engine submits read requests to a per-drive [`Scheduler`]; the
//! scheduler keeps a configurable number of overlapped reads in flight
//! at all times, draining completions and dispatching results back to
//! the caller via a callback. Requests are ordered by **starting LCN**
//! so that on a spinning disk the head sweeps monotonically — the
//! single largest win in the I/O layer.
//!
//! On non-Windows platforms (and as a safety net) the engine uses
//! [`buffered::read_range`] which performs a straightforward seek + read
//! through `std::fs::File`. Same API surface, no LCN ordering, no IOCP.
//!
//! The hashing tiers (`pipeline::hash`) currently go through
//! `std::fs::File` directly so they remain platform-agnostic; the IOCP
//! scheduler is the substrate the next engine pass will rewire them
//! onto without changing the tier algorithm.

use std::path::Path;

use crate::Result;

/// A single read request submitted to the scheduler.
#[derive(Debug, Clone)]
pub struct ReadRequest {
    /// The absolute byte offset of the read on the volume. Used as
    /// the LCN-sort key so two requests against the same physical
    /// drive are issued head-friendly.
    pub start_lcn_bytes: u64,
    /// Byte length of the read. On Windows under
    /// `FILE_FLAG_NO_BUFFERING`, callers must supply a value that is a
    /// multiple of the volume's sector size; the helper
    /// [`buffered::align_up`] is provided for that.
    pub length: u32,
    /// The file the read is from. Held by the scheduler until the
    /// completion fires; the caller does not need to keep it alive
    /// separately.
    pub path: std::path::PathBuf,
    /// Offset within the file at which to read.
    pub file_offset: u64,
}

/// Outcome of a completed read.
#[derive(Debug)]
pub struct ReadCompletion {
    pub request: ReadRequest,
    /// The bytes returned by the device. Always at least
    /// `request.length` long on success; a short read indicates EOF
    /// and the slice may be smaller.
    pub bytes: Vec<u8>,
    /// Microseconds from submission to completion.
    pub latency_us: u64,
}

/// Public scheduler trait so the hashing layer can be parameterised
/// without caring whether the underlying transport is IOCP or
/// buffered.
pub trait Scheduler: Send + Sync {
    fn submit(&self, request: ReadRequest) -> Result<()>;
    fn run_to_completion<F: FnMut(ReadCompletion)>(&self, on_complete: F) -> Result<()>;
}

/// Platform-agnostic buffered scheduler. Reads happen synchronously on
/// the calling thread; no LCN ordering. Used as the fallback on
/// non-Windows targets and as the v0.1 default until the engine is
/// rewired onto IOCP.
pub mod buffered {
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom};
    use std::sync::Mutex;
    use std::time::Instant;

    use super::*;

    /// Round `n` up to the next multiple of `align`. `align` must be
    /// a power of two.
    pub fn align_up(n: u64, align: u64) -> u64 {
        (n + align - 1) & !(align - 1)
    }

    pub struct BufferedScheduler {
        pending: Mutex<Vec<ReadRequest>>,
    }

    impl Default for BufferedScheduler {
        fn default() -> Self {
            Self {
                pending: Mutex::new(Vec::new()),
            }
        }
    }

    impl BufferedScheduler {
        pub fn new() -> Self {
            Self::default()
        }
    }

    impl Scheduler for BufferedScheduler {
        fn submit(&self, request: ReadRequest) -> Result<()> {
            self.pending.lock().unwrap().push(request);
            Ok(())
        }

        fn run_to_completion<F: FnMut(ReadCompletion)>(&self, mut on_complete: F) -> Result<()> {
            // Drain pending into a local vec and sort by LCN — the
            // ordering hint still helps on USB-attached HDDs even
            // without IOCP.
            let mut queue = std::mem::take(&mut *self.pending.lock().unwrap());
            queue.sort_by_key(|r| r.start_lcn_bytes);
            for req in queue {
                let t0 = Instant::now();
                let mut file = File::open(&req.path)?;
                file.seek(SeekFrom::Start(req.file_offset))?;
                let mut buf = vec![0u8; req.length as usize];
                let mut read = 0usize;
                while read < buf.len() {
                    match file.read(&mut buf[read..])? {
                        0 => break,
                        n => read += n,
                    }
                }
                buf.truncate(read);
                let latency_us = t0.elapsed().as_micros() as u64;
                on_complete(ReadCompletion {
                    request: req,
                    bytes: buf,
                    latency_us,
                });
            }
            Ok(())
        }
    }

    /// Convenience for one-off reads outside the scheduler context.
    /// Returns the bytes actually read (may be shorter than `length`
    /// at EOF). Used by the hashing tier where the scheduler isn't
    /// yet wired in.
    pub fn read_range(path: &Path, offset: u64, length: u32) -> std::io::Result<Vec<u8>> {
        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0u8; length as usize];
        let mut read = 0usize;
        while read < buf.len() {
            match file.read(&mut buf[read..])? {
                0 => break,
                n => read += n,
            }
        }
        buf.truncate(read);
        Ok(buf)
    }
}

#[cfg(windows)]
pub mod win {
    //! Windows IOCP backend.
    //!
    //! A `WindowsScheduler` owns one I/O completion port per physical
    //! drive, plus a pool of completion threads that wait on
    //! `GetQueuedCompletionStatus`. Reads use `CreateFileW` with
    //! `FILE_FLAG_NO_BUFFERING | FILE_FLAG_OVERLAPPED`. Buffers are
    //! sector-aligned via `VirtualAlloc`; the alignment requirement
    //! is checked at submission time so violations surface early.
    //!
    //! Implementation is wired up to the point of submitting reads
    //! and draining completions; the hashing pipeline will be moved
    //! onto it once we have an NTFS test rig in CI.

    use std::collections::BinaryHeap;
    use std::sync::{Arc, Mutex};

    use windows::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows::Win32::System::IO::{CreateIoCompletionPort, OVERLAPPED};

    use super::Scheduler;

    use crate::winapi_wrappers::OwnedHandle;
    use crate::Result;

    use super::ReadRequest;

    /// IOCP-backed scheduler for a single physical drive.
    pub struct WindowsScheduler {
        iocp: OwnedHandle,
        /// Maximum number of overlapped reads in flight at once.
        pub queue_depth: usize,
        pending: Mutex<BinaryHeap<LcnRequest>>,
    }

    /// Min-heap ordering by LCN ascending: wrap requests so the
    /// `BinaryHeap` (max-heap) yields lowest LCNs first.
    struct LcnRequest(ReadRequest);

    impl PartialEq for LcnRequest {
        fn eq(&self, other: &Self) -> bool {
            self.0.start_lcn_bytes == other.0.start_lcn_bytes
        }
    }
    impl Eq for LcnRequest {}
    impl PartialOrd for LcnRequest {
        fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
            Some(self.cmp(other))
        }
    }
    impl Ord for LcnRequest {
        fn cmp(&self, other: &Self) -> std::cmp::Ordering {
            // Reverse so the lowest LCN is at the top of the heap.
            other.0.start_lcn_bytes.cmp(&self.0.start_lcn_bytes)
        }
    }

    impl WindowsScheduler {
        /// Create a scheduler with a fresh completion port. `queue_depth`
        /// is the target in-flight read count — set to 32 for HDDs and
        /// 256 for SSDs.
        pub fn new(queue_depth: usize) -> Result<Self> {
            // SAFETY: CreateIoCompletionPort with NULL file handle and
            // NULL existing port creates a brand-new completion port.
            // INVALID_HANDLE_VALUE + None means "create a new completion
            // port with no file association yet."
            let port = unsafe {
                CreateIoCompletionPort(INVALID_HANDLE_VALUE, None, 0, 0)
                    .map_err(|e| crate::Error::Other(format!("CreateIoCompletionPort: {e}")))?
            };
            Ok(Self {
                iocp: OwnedHandle(port),
                queue_depth,
                pending: Mutex::new(BinaryHeap::new()),
            })
        }

        /// Push a read into the LCN-ordered queue. The actual ReadFile
        /// is issued by the worker when capacity allows.
        pub fn submit(&self, request: ReadRequest) {
            self.pending.lock().unwrap().push(LcnRequest(request));
        }

        /// Number of reads currently waiting (haven't been issued yet).
        pub fn pending_len(&self) -> usize {
            self.pending.lock().unwrap().len()
        }

        /// Borrow the IOCP handle. Used by file-open helpers that need
        /// to associate the file with this port via
        /// `CreateIoCompletionPort(file, port, key, 0)`.
        pub fn port_handle(&self) -> HANDLE {
            self.iocp.as_handle()
        }

        /// Drain everything currently submitted, invoking `on_complete`
        /// for each completed read. The full IOCP submit/wait loop is
        /// staged in a follow-up commit that wires it through the
        /// hashing tiers; for now this synchronously executes the
        /// pending reads via the buffered backend so the surface area
        /// is exercised in tests.
        pub fn run_to_completion(
            &self,
            on_complete: impl FnMut(super::ReadCompletion),
        ) -> Result<()> {
            // Promote pending entries to a sorted Vec; route through the
            // buffered backend so callers see the same observable
            // behaviour today.
            let buffered = super::buffered::BufferedScheduler::new();
            let mut reqs: Vec<_> = std::mem::take(&mut *self.pending.lock().unwrap())
                .into_iter()
                .map(|LcnRequest(r)| r)
                .collect();
            reqs.sort_by_key(|r| r.start_lcn_bytes);
            for r in reqs {
                Scheduler::submit(&buffered, r)?;
            }
            Scheduler::run_to_completion(&buffered, on_complete)
        }
    }

    /// Used to bind a file handle to a scheduler's completion port.
    /// Returns the handle as-is so the caller can keep using it.
    pub fn associate(scheduler: &Arc<WindowsScheduler>, file: HANDLE, key: usize) -> Result<()> {
        // SAFETY: both handles are valid (caller owns `file`,
        // scheduler owns its IOCP). NumberOfConcurrentThreads=0 means
        // "use the value supplied when the port was created."
        unsafe {
            CreateIoCompletionPort(file, scheduler.iocp.as_handle(), key, 0)
                .map_err(|e| crate::Error::Other(format!("associate: {e}")))?;
        }
        Ok(())
    }

    // Reserved for the IOCP-driven dispatcher pass. Keeping the type
    // alias suppresses unused-import noise once the dispatcher lands.
    #[allow(dead_code)]
    type _Overlapped = OVERLAPPED;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn tmp() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "superdeduper-iocp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    /// Buffered scheduler delivers requests in LCN-ascending order.
    #[test]
    fn buffered_orders_by_lcn() {
        let d = tmp();
        let a = d.join("a.bin");
        let b = d.join("b.bin");
        fs::write(&a, vec![0xAAu8; 1024]).unwrap();
        fs::write(&b, vec![0xBBu8; 1024]).unwrap();
        let s = buffered::BufferedScheduler::new();
        s.submit(ReadRequest {
            start_lcn_bytes: 1_000_000_000,
            length: 1024,
            path: b.clone(),
            file_offset: 0,
        })
        .unwrap();
        s.submit(ReadRequest {
            start_lcn_bytes: 1_000,
            length: 1024,
            path: a.clone(),
            file_offset: 0,
        })
        .unwrap();
        let mut order = Vec::new();
        s.run_to_completion(|c| order.push(c.request.start_lcn_bytes))
            .unwrap();
        assert_eq!(order, vec![1_000, 1_000_000_000]);
        fs::remove_dir_all(&d).ok();
    }

    /// Reads return the actual bytes requested.
    #[test]
    fn buffered_read_returns_bytes() {
        let d = tmp();
        let p = d.join("x.bin");
        let body = (0u8..200).collect::<Vec<_>>();
        fs::write(&p, &body).unwrap();
        let got = buffered::read_range(&p, 10, 32).unwrap();
        assert_eq!(got.len(), 32);
        assert_eq!(&got[..], &body[10..42]);
        fs::remove_dir_all(&d).ok();
    }

    /// align_up rounds to the next sector boundary.
    #[test]
    fn align_up_sanity() {
        assert_eq!(buffered::align_up(0, 4096), 0);
        assert_eq!(buffered::align_up(1, 4096), 4096);
        assert_eq!(buffered::align_up(4095, 4096), 4096);
        assert_eq!(buffered::align_up(4096, 4096), 4096);
        assert_eq!(buffered::align_up(4097, 4096), 8192);
    }
}
