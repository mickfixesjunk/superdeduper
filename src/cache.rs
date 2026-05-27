//! Per-machine persistent cache, keyed by `(volume_guid, file_ref)` and
//! invalidated whenever `(size, mtime, usn)` changes for a record.
//!
//! `file_ref` is the inode-equivalent identifier — `GetFileInformationByHandle`'s
//! `nFileIndex` on Windows, `st_ino` on Linux/macOS. `volume_guid` is
//! the volume identity — the `\\?\Volume{…}` GUID on Windows,
//! `"linux-dev-{st_dev}"` on Unix. The keying is intentionally
//! filesystem-identity-based, NOT path-based: if a file is renamed,
//! moved, or hardlinked to a new path, the cache still hits. If a
//! file is replaced with a fresh inode (e.g. `rm + cp -a`, or
//! restoring a backup), the cache MISSES even when path + mtime +
//! size all match — fresh inode → fresh hash. See
//! [`tests/cache_corpus_reset`] for the regression that pins this
//! (#36).
//!
//! The cache lives at `%LOCALAPPDATA%\superdeduper\cache.db` on Windows
//! and `$XDG_CACHE_HOME/superdeduper/cache.db` (or `~/.cache/superdeduper/`)
//! elsewhere — the non-Windows path exists only so the cross-platform
//! tests have somewhere to write.
//!
//! Schema and column meanings follow the project spec exactly.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

use crate::pipeline::hash::HashAlgo;
use crate::{Error, Result};

/// The bundled cache schema.
/// * v2 added the `hash_algo` column so the same volume+file_ref can
///   carry separate rows for Blake3 and the 128-bit hash (their
///   hashes are different bytes for the same file).
/// * v3 was a no-op shape bump after the crate rename to `river5`
///   so pre-rename `"ddh128"`-tagged rows got dropped in one go.
/// * v4 was another no-op shape bump for the river5 v2 → v3 swap.
///   v3 changes per-block mixing so byte outputs diverge from v2
///   even for the same input; bumping clears v2-era `"river5"`
///   rows in one sweep rather than waiting for natural eviction.
/// * v5 added the inventory-snapshot tables (`inventory_meta` +
///   `inventory_records`) so the warm-path Stage 1 enumerator can
///   apply a USN-journal delta against a cached baseline instead
///   of re-walking the whole MFT.
/// * v6 is another no-op shape bump for the river5 v3 → v15
///   promotion. v15 changes the mixer (≈ 2× throughput) and
///   produces byte-different outputs from v3 for the same input;
///   bumping clears v3-era `"river5"` hash rows in one sweep so
///   warm cache lookups can't accidentally pair a v3 hash with a
///   v15 hash and silently report a wrong "duplicate".
/// * v7 adds the `reparse_tag` column to `inventory_records` to
///   persist the cloud-placeholder / reparse-point identity across
///   scans. T2.1 phase 4 (tier guards) consults
///   `PlaceholderState::blocks_content_read()`; that state is
///   derived from `(attributes, reparse_tag)` via
///   `inventory::placeholder::classify()`. Today's USN-data path
///   leaves `reparse_tag = None` (FSCTL_GET_REPARSE_POINT isn't
///   wired up yet), but the column gives us a place to land the
///   real tag once it is, without a future schema bump. NULL means
///   "tag unknown" — `classify(attrs, None)` still produces the
///   safe-default `OtherReparse(0)` for unknown reparses, so warm
///   path correctness is preserved.
///
/// Bumping this string causes init_schema to drop and recreate the
/// tables; any cached data from older versions is discarded.
const SCHEMA_VERSION: &str = "7";

#[derive(Debug, Clone)]
pub struct CacheKey {
    pub volume_guid: String,
    pub file_ref: i64,
    pub size: u64,
    /// 100ns FILETIME ticks.
    pub mtime: i64,
    pub usn: i64,
    /// Which content-hash algorithm produced the cached BLAKE/DDH
    /// bytes. Stored in the primary key so an algo switch never
    /// pulls a stale row.
    pub hash_algo: HashAlgo,
}

#[derive(Debug, Default, Clone)]
pub struct CachedHashes {
    pub tier0_fingerprint: Option<Vec<u8>>,
    pub tier1_hash: Option<Vec<u8>>,
    pub tier2_hash: Option<Vec<u8>>,
    pub tier3_hash: Option<Vec<u8>>,
}

/// #99 PR9 — One row of the warm in-memory cache. Mirrors the
/// SQLite row shape so [`lookup_warm`] can do drift detection
/// without an SQLite query. Use `Cache::warm_load_all` to build
/// the HashMap once at Stage 4 start; pass `Arc<HashMap<_>>`
/// through the hash pipeline.
#[derive(Debug, Clone)]
pub struct WarmCacheEntry {
    pub size: i64,
    pub mtime: i64,
    pub usn: i64,
    pub hashes: CachedHashes,
}

/// #99 PR9 — Lock-free cache lookup against a pre-loaded warm
/// HashMap. Replaces the SQLite-mutex-serialized `Cache::lookup`
/// path during Stage 4 hashing on resume runs. Drift detection
/// (size/mtime/usn mismatch) runs against the warm row's stored
/// values; same semantic as `Cache::lookup_detailed`.
pub fn lookup_warm(
    map: &std::collections::HashMap<(String, i64), WarmCacheEntry>,
    key: &CacheKey,
) -> LookupOutcome {
    let warm_key = (key.volume_guid.clone(), key.file_ref);
    match map.get(&warm_key) {
        None => LookupOutcome::NoRow,
        Some(entry) => {
            if entry.size as u64 != key.size {
                return LookupOutcome::Drift {
                    reason: DriftReason::Size,
                };
            }
            if entry.mtime != key.mtime {
                return LookupOutcome::Drift {
                    reason: DriftReason::Mtime,
                };
            }
            if entry.usn != key.usn {
                return LookupOutcome::Drift {
                    reason: DriftReason::Usn,
                };
            }
            LookupOutcome::Hit(entry.hashes.clone())
        }
    }
}

/// #99 PR3 — Cache lookup outcome with the structural detail
/// `lookup()`'s `Option<CachedHashes>` flattens away. Distinguishes
/// "no row" from "row exists but file drifted"; the hash pipeline
/// uses Drift detection to tally per-file re-validation events for
/// the UI status surface (resume runs no longer silently
/// restart-from-zero on USN advance per #52).
#[derive(Debug, Clone)]
pub enum LookupOutcome {
    /// Cache row matches all (size, mtime, usn) — hashes returned.
    Hit(CachedHashes),
    /// Cache row exists but key fields changed since last hash.
    /// File will be re-hashed; bump the drift counter.
    Drift { reason: DriftReason },
    /// No cache row for this `(volume_guid, file_ref, hash_algo)`.
    /// File has either never been hashed or its inode was reused.
    NoRow,
}

/// Which axis of (size, mtime, usn) caused the cache invalidation.
/// Checked in order; first mismatch wins. Future per-axis breakdown
/// could be useful — e.g., counting USN-only drift vs mtime-touch
/// vs size-change separately would let us flag "files that grew"
/// (typical: log append) vs "files that shrank" (typical: truncate).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriftReason {
    /// File size changed since last hash. Strongest signal of
    /// real-content change.
    Size,
    /// File mtime advanced; size unchanged. Could be "touched
    /// without modification" (rare) or "modified to same length
    /// with different content."
    Mtime,
    /// USN-journal sequence advanced; size + mtime unchanged.
    /// Windows-specific signal that any of a broader set of
    /// attributes changed (metadata, attributes flag, security
    /// descriptor); content may or may not have changed.
    Usn,
}

/// Persistent companion to the warm-path Stage 1 enumerator.
/// `journal_id` and `last_usn` together identify "the journal state
/// at the moment we last enumerated this volume". When we come back
/// for a rescan, we ask Win32 for the current journal state — if
/// `journal_id` still matches and `last_usn` is still inside the
/// journal's live range, applying a delta from `last_usn` is safe.
#[derive(Debug, Clone)]
pub struct InventoryMeta {
    pub journal_id: i64,
    pub last_usn: i64,
    pub captured_at_unix: i64,
}

/// One persisted MFT record. We store directories AND files in the
/// same table — directories are needed for `reconstruct_path`'s
/// parent-chain walk, files are the actual scan output. Directories
/// use `size = -1`, `mtime = 0` as sentinels; the warm-path
/// enumerator filters them out before returning `FileEntry`s.
#[derive(Debug, Clone)]
pub struct InventoryRecord {
    pub parent_ref: u64,
    pub usn: i64,
    pub attributes: u32,
    pub name: String,
    pub size: i64,
    pub mtime: u64,
    /// Reparse tag for files where `attributes & FILE_ATTRIBUTE_REPARSE_POINT`
    /// is set. `None` means "not a reparse point OR tag not yet fetched."
    /// Cold-path producers leave this `None` today (FSCTL_GET_REPARSE_POINT
    /// isn't wired up yet); the warm path passes it through to
    /// `placeholder::classify()` so the tier guard sees the same state on
    /// resume that it saw on the original scan once the fetcher lands.
    pub reparse_tag: Option<u32>,
}

impl InventoryRecord {
    /// `FILE_ATTRIBUTE_DIRECTORY = 0x10`. The check is duplicated in
    /// a couple of places — pulled into one constant so future
    /// MFT-record refactors only touch this file.
    pub fn is_directory(&self) -> bool {
        (self.attributes & 0x10) != 0
    }
}

impl CachedHashes {
    pub fn is_empty(&self) -> bool {
        self.tier0_fingerprint.is_none()
            && self.tier1_hash.is_none()
            && self.tier2_hash.is_none()
            && self.tier3_hash.is_none()
    }
}

pub struct Cache {
    conn: Connection,
    /// #99 PR9 — In-memory bulk-loaded mirror of the hash-cache
    /// table for one hash algorithm. Populated by `warm_in_place`
    /// at Stage 4 start. When present, `lookup_detailed` checks it
    /// before issuing the SQLite SELECT — turning ~100-500µs
    /// queries into ~100-500ns HashMap lookups.
    ///
    /// Read-only after build. Writes (`store`) still update
    /// SQLite; the in-memory map is not updated mid-scan because
    /// (a) the current scan's writes target files we just hashed,
    /// not files we're about to look up again, and (b) keeping it
    /// read-only means no second locking primitive is needed
    /// (the Cache's outer Mutex already serializes).
    warm_map: Option<std::collections::HashMap<(String, i64), WarmCacheEntry>>,
}

#[derive(Debug, Clone)]
pub struct CacheStats {
    pub path: PathBuf,
    pub rows: u64,
    /// Row count of the inventory-snapshot table (used by the
    /// warm-path Stage 1 inventory). Distinct from `rows` (hash
    /// cache) — a heavy snapshot can persist even when hash rows
    /// have been cleared.
    pub snapshot_rows: u64,
    pub bytes_on_disk: u64,
}

impl Cache {
    /// Open (or create) the cache at the default location.
    pub fn open_default() -> Result<Self> {
        Self::open(&default_cache_path()?)
    }

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // #106 PR1 — busy_timeout. Without this, a concurrent writer
        // racing with our connection returns SQLITE_BUSY immediately
        // and we surface the error as a cache write-failure (now
        // tracked via cache_write_failures counter). 5s is SQLite's
        // community default — long enough that ordinary contention
        // resolves, short enough that a genuinely stuck DB doesn't
        // freeze the scan.
        conn.pragma_update(None, "busy_timeout", "5000")?;

        let cache = Cache {
            conn,
            warm_map: None,
        };
        cache.init_schema()?;
        Ok(cache)
    }

    fn init_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );",
        )?;
        let existing: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .optional()?;

        match existing.as_deref() {
            Some(v) if v == SCHEMA_VERSION => {}
            Some(_) => {
                // Schema mismatch — drop and recreate. List every
                // table we own here so a future schema bump can't
                // leave orphaned rows behind on an older client.
                self.conn.execute_batch(
                    "DROP TABLE IF EXISTS files;
                     DROP TABLE IF EXISTS volumes;
                     DROP TABLE IF EXISTS inventory_meta;
                     DROP TABLE IF EXISTS inventory_records;",
                )?;
            }
            None => {}
        }

        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS volumes (
                volume_guid TEXT PRIMARY KEY,
                last_usn    INTEGER NOT NULL DEFAULT 0,
                last_seen   INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS files (
                volume_guid       TEXT NOT NULL,
                file_ref          INTEGER NOT NULL,
                hash_algo         TEXT NOT NULL,
                size              INTEGER NOT NULL,
                mtime             INTEGER NOT NULL,
                usn               INTEGER NOT NULL,
                tier0_fingerprint BLOB,
                tier1_hash        BLOB,
                tier2_hash        BLOB,
                tier3_hash        BLOB,
                last_seen         INTEGER NOT NULL,
                PRIMARY KEY (volume_guid, file_ref, hash_algo)
            );
            CREATE INDEX IF NOT EXISTS idx_size ON files(size);
            CREATE INDEX IF NOT EXISTS idx_tier3 ON files(tier3_hash) WHERE tier3_hash IS NOT NULL;

            -- v5 inventory-snapshot tables. The warm-path Stage 1
            -- enumerator validates `journal_id` + `last_usn` against
            -- the current FSCTL_QUERY_USN_JOURNAL output to decide
            -- whether a delta scan is safe; if either changed we
            -- nuke the snapshot rows for that volume and fall back
            -- to a cold MFT walk.
            CREATE TABLE IF NOT EXISTS inventory_meta (
                volume_guid       TEXT PRIMARY KEY,
                journal_id        INTEGER NOT NULL,
                last_usn          INTEGER NOT NULL,
                captured_at_unix  INTEGER NOT NULL
            );

            -- One row per MFT record (directories AND files).
            -- Directories carry size = -1 / mtime = 0 since their
            -- only use is the parent-chain lookup that
            -- reconstruct_path() walks.
            -- v7: reparse_tag column. NULL when the record isn't a
            -- reparse point or the tag wasn't fetched at scan time;
            -- the warm-path enumerator passes it through to
            -- placeholder::classify() so the tier guard sees the
            -- same state on resume.
            CREATE TABLE IF NOT EXISTS inventory_records (
                volume_guid TEXT    NOT NULL,
                file_ref    INTEGER NOT NULL,
                parent_ref  INTEGER NOT NULL,
                usn         INTEGER NOT NULL,
                attributes  INTEGER NOT NULL,
                name        TEXT    NOT NULL,
                size        INTEGER NOT NULL,
                mtime       INTEGER NOT NULL,
                reparse_tag INTEGER,
                PRIMARY KEY (volume_guid, file_ref)
            );
            CREATE INDEX IF NOT EXISTS idx_inventory_records_volume
                ON inventory_records (volume_guid);",
        )?;

        self.conn.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES ('schema_version', ?1)",
            params![SCHEMA_VERSION],
        )?;
        Ok(())
    }

    /// Look up cached hashes. Returns `None` if no row matches, OR if
    /// the cached `(size, mtime, usn)` differ from the supplied key —
    /// i.e. the file has changed since we last hashed it.
    pub fn lookup(&self, key: &CacheKey) -> Result<Option<CachedHashes>> {
        match self.lookup_detailed(key)? {
            LookupOutcome::Hit(h) => Ok(Some(h)),
            LookupOutcome::Drift { .. } | LookupOutcome::NoRow => Ok(None),
        }
    }

    /// #99 PR9 — Bulk-load the hash cache into an in-memory mirror
    /// stored on this `Cache`. Single SQL query reads every row for
    /// the given algo into a HashMap; subsequent `lookup_detailed`
    /// calls check the in-memory mirror first and skip the SQLite
    /// SELECT entirely. ~1000x speedup per lookup (mutex acquire
    /// dominates instead of SQLite query).
    ///
    /// Call once at Stage 4 start in `gui/live.rs` on resume runs
    /// where cache fast-forward is expected to dominate.
    ///
    /// Read-only after build. Mid-scan writes via `store` still go
    /// to SQLite but DON'T update the warm map — the in-memory
    /// mirror is a snapshot of what was on disk at warm time.
    pub fn warm_in_place(&mut self, algo: HashAlgo) -> Result<usize> {
        let map = self.warm_load_all(algo)?;
        let n = map.len();
        self.warm_map = Some(map);
        Ok(n)
    }

    /// #99 PR11 — Walk the supplied keys against the warm map and
    /// return the count of `Hit` outcomes. Used by `gui/live.rs` to
    /// pre-credit the Stage-4 progress bar at frame zero so the bar
    /// JUMPS to the pre-kill position on resume instead of climbing
    /// up to it over the duration of the fast-forward window.
    ///
    /// Cost: one HashMap lookup + size/mtime/usn compare per key,
    /// no SQLite I/O. ~1µs/key → 450k keys = sub-second pre-flight.
    /// If the warm map is None (warm load skipped/failed), returns 0
    /// and the bar falls back to the climb-during-fast-forward UX.
    pub fn predict_hits(&self, keys: &[CacheKey]) -> u64 {
        let Some(ref warm) = self.warm_map else {
            return 0;
        };
        let mut hits: u64 = 0;
        for key in keys {
            if matches!(lookup_warm(warm, key), LookupOutcome::Hit(_)) {
                hits = hits.saturating_add(1);
            }
        }
        hits
    }

    /// #99 PR3 — Same lookup, but distinguishes "no cache row" from
    /// "row exists but file drifted." Powers the per-file
    /// revalidation status surfaced to the UI on resume runs so
    /// users don't see opaque progress restart-from-zero (#52).
    /// The `Drift` variant carries the kind of drift that caused
    /// the invalidation so per-axis counts can be tallied if
    /// future work wants the breakdown.
    ///
    /// #99 PR9 — If `warm_map` is populated (via `warm_in_place`),
    /// check it first; SQLite SELECT is skipped on hit. This is
    /// the resume cache-fast-forward fast path.
    pub fn lookup_detailed(&self, key: &CacheKey) -> Result<LookupOutcome> {
        if let Some(ref warm) = self.warm_map {
            return Ok(lookup_warm(warm, key));
        }
        type Row = (
            i64,             // size
            i64,             // mtime
            i64,             // usn
            Option<Vec<u8>>, // tier0_fingerprint
            Option<Vec<u8>>, // tier1_hash
            Option<Vec<u8>>, // tier2_hash
            Option<Vec<u8>>, // tier3_hash
        );
        let row: Option<Row> = self
            .conn
            .query_row(
                "SELECT size, mtime, usn, tier0_fingerprint, tier1_hash, tier2_hash, tier3_hash
                 FROM files WHERE volume_guid = ?1 AND file_ref = ?2 AND hash_algo = ?3",
                params![key.volume_guid, key.file_ref, key.hash_algo.tag()],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .optional()?;

        let (size, mtime, usn, t0, t1, t2, t3) = match row {
            Some(r) => r,
            None => return Ok(LookupOutcome::NoRow),
        };
        if size as u64 != key.size {
            return Ok(LookupOutcome::Drift {
                reason: DriftReason::Size,
            });
        }
        if mtime != key.mtime {
            return Ok(LookupOutcome::Drift {
                reason: DriftReason::Mtime,
            });
        }
        if usn != key.usn {
            return Ok(LookupOutcome::Drift {
                reason: DriftReason::Usn,
            });
        }

        Ok(LookupOutcome::Hit(CachedHashes {
            tier0_fingerprint: t0,
            tier1_hash: t1,
            tier2_hash: t2,
            tier3_hash: t3,
        }))
    }

    /// #99 PR9 — Bulk-load all cache rows for `hash_algo` into an
    /// in-memory HashMap. Single SQL query; ~100ms for 100k rows.
    /// Use case: at Stage 4 start in `gui/live.rs`, build this
    /// HashMap and pass through the hash pipeline as a lock-free
    /// lookup-first layer. Removes SQLite Mutex serialization
    /// across Rayon workers — per-file lookup throughput jumps
    /// from ~300/sec (mutex-bound) to ~100k/sec (HashMap.get).
    ///
    /// The map's key is `(volume_guid, file_ref)` — same as the
    /// SQL primary key minus `hash_algo` (filtered out via the
    /// WHERE clause; one map per algo is fine).
    ///
    /// Values include `size + mtime + usn` so drift detection
    /// happens in-memory too (no SQLite query needed for the
    /// "file changed since last hash" check at `cache.rs:324`).
    pub fn warm_load_all(
        &self,
        algo: HashAlgo,
    ) -> Result<std::collections::HashMap<(String, i64), WarmCacheEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT volume_guid, file_ref, size, mtime, usn,
                    tier0_fingerprint, tier1_hash, tier2_hash, tier3_hash
             FROM files WHERE hash_algo = ?1",
        )?;
        let mut rows = stmt.query(params![algo.tag()])?;
        let mut map = std::collections::HashMap::new();
        while let Some(row) = rows.next()? {
            let vg: String = row.get(0)?;
            let fr: i64 = row.get(1)?;
            let size: i64 = row.get(2)?;
            let mtime: i64 = row.get(3)?;
            let usn: i64 = row.get(4)?;
            let t0: Option<Vec<u8>> = row.get(5)?;
            let t1: Option<Vec<u8>> = row.get(6)?;
            let t2: Option<Vec<u8>> = row.get(7)?;
            let t3: Option<Vec<u8>> = row.get(8)?;
            map.insert(
                (vg, fr),
                WarmCacheEntry {
                    size,
                    mtime,
                    usn,
                    hashes: CachedHashes {
                        tier0_fingerprint: t0,
                        tier1_hash: t1,
                        tier2_hash: t2,
                        tier3_hash: t3,
                    },
                },
            );
        }
        Ok(map)
    }

    pub fn store(&self, key: &CacheKey, hashes: &CachedHashes) -> Result<()> {
        let now = now_unix();
        // Tier columns use COALESCE on conflict so a single-tier
        // write doesn't clobber the others. The hash pipeline calls
        // `store` once per tier with only that tier populated; the
        // earlier "set every column to excluded.X" version meant the
        // last tier stored erased the prior ones, so on resume only
        // the deepest tier (typically Tier 3) survived and Tiers 1-2
        // re-ran from scratch. With COALESCE, NULL-in-the-write
        // preserves the existing column. Metadata (size/mtime/usn)
        // still replaces because those are scoped to this file's
        // current physical state.
        self.conn.execute(
            "INSERT INTO files (
                volume_guid, file_ref, hash_algo, size, mtime, usn,
                tier0_fingerprint, tier1_hash, tier2_hash, tier3_hash, last_seen
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(volume_guid, file_ref, hash_algo) DO UPDATE SET
                size = excluded.size,
                mtime = excluded.mtime,
                usn = excluded.usn,
                tier0_fingerprint = COALESCE(excluded.tier0_fingerprint, tier0_fingerprint),
                tier1_hash        = COALESCE(excluded.tier1_hash,        tier1_hash),
                tier2_hash        = COALESCE(excluded.tier2_hash,        tier2_hash),
                tier3_hash        = COALESCE(excluded.tier3_hash,        tier3_hash),
                last_seen = excluded.last_seen",
            params![
                key.volume_guid,
                key.file_ref,
                key.hash_algo.tag(),
                key.size as i64,
                key.mtime,
                key.usn,
                hashes.tier0_fingerprint.as_deref(),
                hashes.tier1_hash.as_deref(),
                hashes.tier2_hash.as_deref(),
                hashes.tier3_hash.as_deref(),
                now,
            ],
        )?;
        Ok(())
    }

    /// Update or insert the last-known-good USN for a volume. Use after
    /// a successful scan so the next run can ask the USN journal for a
    /// delta instead of re-reading every file.
    pub fn set_volume_usn(&self, volume_guid: &str, usn: i64) -> Result<()> {
        let now = now_unix();
        self.conn.execute(
            "INSERT INTO volumes(volume_guid, last_usn, last_seen) VALUES (?1, ?2, ?3)
             ON CONFLICT(volume_guid) DO UPDATE SET last_usn = excluded.last_usn, last_seen = excluded.last_seen",
            params![volume_guid, usn, now],
        )?;
        Ok(())
    }

    pub fn get_volume_usn(&self, volume_guid: &str) -> Result<Option<i64>> {
        let v: Option<i64> = self
            .conn
            .query_row(
                "SELECT last_usn FROM volumes WHERE volume_guid = ?1",
                params![volume_guid],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v)
    }

    pub fn clear(&self) -> Result<()> {
        self.conn.execute("DELETE FROM files", [])?;
        self.conn.execute("DELETE FROM volumes", [])?;
        self.conn.execute("DELETE FROM inventory_meta", [])?;
        self.conn.execute("DELETE FROM inventory_records", [])?;
        Ok(())
    }

    /// Read the saved inventory-meta block for a volume. `None` ⇒ no
    /// snapshot for this volume yet (or the warm path explicitly
    /// invalidated it). Caller pairs this with
    /// `winapi_wrappers::query_usn_journal_state` to decide whether
    /// the snapshot is still useful — if `journal_id` doesn't match
    /// or `last_usn` is older than the journal's current `first_usn`,
    /// the journal wrapped and we must cold-scan again.
    pub fn load_inventory_meta(&self, volume_guid: &str) -> Result<Option<InventoryMeta>> {
        let row = self
            .conn
            .query_row(
                "SELECT journal_id, last_usn, captured_at_unix
                 FROM inventory_meta WHERE volume_guid = ?1",
                params![volume_guid],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?;
        Ok(
            row.map(|(journal_id, last_usn, captured_at_unix)| InventoryMeta {
                journal_id,
                last_usn,
                captured_at_unix,
            }),
        )
    }

    /// Read every persisted MFT record for a volume. Returns them as
    /// `(file_ref, InventoryRecord)` pairs the caller can drop
    /// straight into a `HashMap<u64, InventoryRecord>` for the path
    /// reconstruction loop. Memory cost ≈ 100 bytes/row × N rows;
    /// 500k AppData files ≈ 50 MB, well within budget.
    pub fn load_inventory_records(&self, volume_guid: &str) -> Result<Vec<(u64, InventoryRecord)>> {
        let mut stmt = self.conn.prepare(
            "SELECT file_ref, parent_ref, usn, attributes, name, size, mtime, reparse_tag
             FROM inventory_records WHERE volume_guid = ?1",
        )?;
        let rows = stmt.query_map(params![volume_guid], |r| {
            Ok((
                r.get::<_, i64>(0)? as u64,
                InventoryRecord {
                    parent_ref: r.get::<_, i64>(1)? as u64,
                    usn: r.get::<_, i64>(2)?,
                    attributes: r.get::<_, i64>(3)? as u32,
                    name: r.get::<_, String>(4)?,
                    size: r.get::<_, i64>(5)?,
                    mtime: r.get::<_, i64>(6)? as u64,
                    reparse_tag: r.get::<_, Option<i64>>(7)?.map(|v| v as u32),
                },
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Overwrite the snapshot for `volume_guid` with the supplied
    /// records + cursor in a single transaction. Stale rows for the
    /// volume are deleted up front so callers don't need to compute
    /// per-row diffs against the existing snapshot.
    pub fn save_inventory_snapshot(
        &mut self,
        volume_guid: &str,
        meta: &InventoryMeta,
        records: &[(u64, InventoryRecord)],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM inventory_records WHERE volume_guid = ?1",
            params![volume_guid],
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO inventory_meta
                 (volume_guid, journal_id, last_usn, captured_at_unix)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                volume_guid,
                meta.journal_id,
                meta.last_usn,
                meta.captured_at_unix
            ],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO inventory_records
                     (volume_guid, file_ref, parent_ref, usn, attributes, name, size, mtime, reparse_tag)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            for (file_ref, rec) in records {
                stmt.execute(params![
                    volume_guid,
                    *file_ref as i64,
                    rec.parent_ref as i64,
                    rec.usn,
                    rec.attributes as i64,
                    rec.name,
                    rec.size,
                    rec.mtime as i64,
                    rec.reparse_tag.map(|t| t as i64),
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Apply a tiny delta to an existing snapshot instead of
    /// rewriting all 2M+ rows. The warm-path enumerator built
    /// this delta itself (created/updated/deleted file_refs) —
    /// passing it through avoids the 60-90s full-rewrite that
    /// `save_inventory_snapshot` does. Typical inter-scan churn
    /// is a few thousand records → the apply is sub-second.
    ///
    /// Behaviour:
    /// * `deletes`: rows removed via `DELETE … WHERE volume_guid = ?
    ///   AND file_ref = ?`.
    /// * `upserts`: rows inserted via `INSERT OR REPLACE …`. The
    ///   warm path uses this for both new-file events (created)
    ///   and modified-file events (updated) since the SQL semantics
    ///   are the same.
    /// * `meta` cursor + journal_id always updated.
    ///
    /// Single transaction, so a crash mid-apply leaves the previous
    /// snapshot intact and the warm path's journal-id check will
    /// still validate the old cursor.
    pub fn apply_inventory_delta(
        &mut self,
        volume_guid: &str,
        meta: &InventoryMeta,
        upserts: &[(u64, InventoryRecord)],
        deletes: &[u64],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO inventory_meta
                 (volume_guid, journal_id, last_usn, captured_at_unix)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                volume_guid,
                meta.journal_id,
                meta.last_usn,
                meta.captured_at_unix
            ],
        )?;
        if !deletes.is_empty() {
            let mut del = tx.prepare(
                "DELETE FROM inventory_records
                 WHERE volume_guid = ?1 AND file_ref = ?2",
            )?;
            for file_ref in deletes {
                del.execute(params![volume_guid, *file_ref as i64])?;
            }
        }
        if !upserts.is_empty() {
            let mut up = tx.prepare(
                "INSERT OR REPLACE INTO inventory_records
                     (volume_guid, file_ref, parent_ref, usn, attributes, name, size, mtime, reparse_tag)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            for (file_ref, rec) in upserts {
                up.execute(params![
                    volume_guid,
                    *file_ref as i64,
                    rec.parent_ref as i64,
                    rec.usn,
                    rec.attributes as i64,
                    rec.name,
                    rec.size,
                    rec.mtime as i64,
                    rec.reparse_tag.map(|t| t as i64),
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Drop the snapshot for one volume — used when journal
    /// validation fails so the next scan starts clean.
    pub fn invalidate_inventory_snapshot(&self, volume_guid: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM inventory_meta WHERE volume_guid = ?1",
            params![volume_guid],
        )?;
        self.conn.execute(
            "DELETE FROM inventory_records WHERE volume_guid = ?1",
            params![volume_guid],
        )?;
        Ok(())
    }

    pub fn vacuum(&self) -> Result<()> {
        self.conn.execute("VACUUM", [])?;
        Ok(())
    }

    /// Lightweight "is there an inventory snapshot for this volume?"
    /// check used by the GUI's "Cache available" banner. Returns
    /// `None` if no `inventory_meta` row matches; returns
    /// `Some((captured_at_unix, record_count))` otherwise. The
    /// record count is cheap because `inventory_records.volume_guid`
    /// is indexed.
    ///
    /// Per-folder granularity (was the scan root previously covered?)
    /// would need a separate `scan_roots` table or bulk-path
    /// reconstruction — both are heavier than we want for a banner
    /// query that runs on every scan-roots-changed event. Per-volume
    /// is conservative in the right direction: it triggers more
    /// often than strictly necessary, but using the cache when it's
    /// not actually relevant is a no-op (cache misses just become
    /// cold reads), so there's no harm in the false positive.
    pub fn cache_summary_for_volume(&self, volume_guid: &str) -> Result<Option<(i64, u64)>> {
        let meta: Option<i64> = self
            .conn
            .query_row(
                "SELECT captured_at_unix FROM inventory_meta WHERE volume_guid = ?",
                [volume_guid],
                |r| r.get(0),
            )
            .optional()?;
        let Some(captured_at) = meta else {
            return Ok(None);
        };
        let count: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM inventory_records WHERE volume_guid = ?",
                [volume_guid],
                |r| r.get(0),
            )
            .unwrap_or(0);
        Ok(Some((captured_at, count as u64)))
    }

    pub fn stats(&self, path: &Path) -> Result<CacheStats> {
        let rows: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?;
        // Inventory-snapshot row count surfaces the warm-path
        // state separately from hash-cache rows. Without this,
        // `cache clear` looks like a no-op when the snapshot is
        // a few hundred MB but logically empty.
        let snapshot_rows: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM inventory_records", [], |r| r.get(0))
            .unwrap_or(0);
        let bytes_on_disk = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        Ok(CacheStats {
            path: path.to_path_buf(),
            rows: rows as u64,
            snapshot_rows: snapshot_rows as u64,
            bytes_on_disk,
        })
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// #99 PR2 — Read-only schema-version observation. Tells the
/// caller whether the next [`Cache::open`] will trigger the
/// SCHEMA_VERSION-mismatch wipe (init_schema:202-212) WITHOUT
/// actually opening the cache + triggering the wipe.
///
/// Used by [`crate::gui::resume_tier::SessionContext`] to decide
/// whether to classify a resume as `Warm` (cold cache) vs `Full`
/// (cache should still be hot). Pure observation; opens the db
/// read-only, queries `meta`, closes. No side effects.
pub fn schema_state(path: &Path) -> Result<SchemaState> {
    if !path.exists() {
        return Ok(SchemaState::NoCache);
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let existing: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    Ok(match existing.as_deref() {
        Some(v) if v == SCHEMA_VERSION => SchemaState::Current,
        Some(_) => SchemaState::Mismatch,
        None => SchemaState::Uninitialized,
    })
}

/// Outcome of [`schema_state`] — tells classify-time callers
/// whether the cache will survive the next `Cache::open`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaState {
    /// SCHEMA_VERSION matches; cache is hot, `Cache::open` won't
    /// wipe it.
    Current,
    /// SCHEMA_VERSION drifted (engine upgraded since last scan).
    /// `Cache::open` WILL wipe + recreate the tables;
    /// `cache_hit_ratio` for the next scan will be ~0.
    Mismatch,
    /// `meta` row exists but `schema_version` key isn't set.
    /// First-time-after-open shape; rare in practice. Treated as
    /// "cold cache, won't wipe but won't help either."
    Uninitialized,
    /// Cache file doesn't exist (fresh install, manually deleted).
    /// Equivalent to a cold cache; classify treats as
    /// schema-mismatch-equivalent because cache lookups will all
    /// miss on the next scan.
    NoCache,
}

impl SchemaState {
    /// `true` when the next scan will operate against an effectively
    /// cold cache (either freshly wiped or freshly created or empty).
    /// Powers the `SessionContext::schema_version_mismatch` signal.
    pub fn implies_cold_cache(self) -> bool {
        matches!(
            self,
            SchemaState::Mismatch | SchemaState::NoCache | SchemaState::Uninitialized
        )
    }
}

#[cfg(windows)]
pub fn default_cache_path() -> Result<PathBuf> {
    let local = std::env::var("LOCALAPPDATA").map_err(|_| Error::EnvVarMissing("LOCALAPPDATA"))?;
    Ok(PathBuf::from(local).join("superdeduper").join("cache.db"))
}

#[cfg(not(windows))]
pub fn default_cache_path() -> Result<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(xdg).join("superdeduper").join("cache.db"));
    }
    let home = std::env::var("HOME").map_err(|_| Error::EnvVarMissing("HOME"))?;
    Ok(PathBuf::from(home)
        .join(".cache")
        .join("superdeduper")
        .join("cache.db"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_db() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "superdeduper-cache-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        p
    }

    fn key(id: i64, size: u64, mtime: i64, usn: i64) -> CacheKey {
        CacheKey {
            volume_guid: "vol:test".into(),
            file_ref: id,
            size,
            mtime,
            usn,
            hash_algo: HashAlgo::Blake3,
        }
    }

    #[test]
    fn store_and_lookup_roundtrip() {
        let p = tmp_db();
        let cache = Cache::open(&p).unwrap();
        let k = key(42, 1024, 100_000, 7);
        let hashes = CachedHashes {
            tier3_hash: Some(vec![0xABu8; 32]),
            ..CachedHashes::default()
        };
        cache.store(&k, &hashes).unwrap();

        let got = cache.lookup(&k).unwrap().expect("row should exist");
        assert_eq!(got.tier3_hash, Some(vec![0xABu8; 32]));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn lookup_misses_on_size_change() {
        let p = tmp_db();
        let cache = Cache::open(&p).unwrap();
        let k = key(42, 1024, 100_000, 7);
        let hashes = CachedHashes {
            tier3_hash: Some(vec![0xABu8; 32]),
            ..CachedHashes::default()
        };
        cache.store(&k, &hashes).unwrap();

        let modified = key(42, 2048, 100_000, 7);
        assert!(cache.lookup(&modified).unwrap().is_none());
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn lookup_misses_on_mtime_change() {
        let p = tmp_db();
        let cache = Cache::open(&p).unwrap();
        let k = key(42, 1024, 100_000, 7);
        let hashes = CachedHashes {
            tier3_hash: Some(vec![0xABu8; 32]),
            ..CachedHashes::default()
        };
        cache.store(&k, &hashes).unwrap();
        let modified = key(42, 1024, 100_001, 7);
        assert!(cache.lookup(&modified).unwrap().is_none());
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn lookup_misses_on_usn_change() {
        let p = tmp_db();
        let cache = Cache::open(&p).unwrap();
        let k = key(42, 1024, 100_000, 7);
        let hashes = CachedHashes {
            tier3_hash: Some(vec![0xABu8; 32]),
            ..CachedHashes::default()
        };
        cache.store(&k, &hashes).unwrap();
        let modified = key(42, 1024, 100_000, 8);
        assert!(cache.lookup(&modified).unwrap().is_none());
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn lookup_detailed_distinguishes_no_row_from_drift_kinds() {
        // #99 PR3 — verify the structured outcome the hash pipeline
        // uses to bump cache_drift_misses (vs no-row, which doesn't
        // signal "file changed since last hash").
        let p = tmp_db();
        let cache = Cache::open(&p).unwrap();
        let k = key(42, 1024, 100_000, 7);
        let hashes = CachedHashes {
            tier3_hash: Some(vec![0xABu8; 32]),
            ..CachedHashes::default()
        };
        cache.store(&k, &hashes).unwrap();

        // Hit — exact match.
        match cache.lookup_detailed(&k).unwrap() {
            LookupOutcome::Hit(h) => assert_eq!(h.tier3_hash, Some(vec![0xABu8; 32])),
            other => panic!("expected Hit, got {other:?}"),
        }

        // Size drift.
        let size_drift = key(42, 2048, 100_000, 7);
        assert!(
            matches!(
                cache.lookup_detailed(&size_drift).unwrap(),
                LookupOutcome::Drift {
                    reason: DriftReason::Size
                }
            ),
            "size change must classify as Drift{{Size}}"
        );

        // Mtime drift.
        let mtime_drift = key(42, 1024, 100_001, 7);
        assert!(
            matches!(
                cache.lookup_detailed(&mtime_drift).unwrap(),
                LookupOutcome::Drift {
                    reason: DriftReason::Mtime
                }
            ),
            "mtime change must classify as Drift{{Mtime}}"
        );

        // USN drift — #52's primary signal.
        let usn_drift = key(42, 1024, 100_000, 8);
        assert!(
            matches!(
                cache.lookup_detailed(&usn_drift).unwrap(),
                LookupOutcome::Drift {
                    reason: DriftReason::Usn
                }
            ),
            "usn change must classify as Drift{{Usn}}"
        );

        // No row — different file_ref.
        let missing = key(99, 1024, 100_000, 7);
        assert!(matches!(
            cache.lookup_detailed(&missing).unwrap(),
            LookupOutcome::NoRow
        ));

        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn predict_hits_counts_only_hits_not_drift_or_norow() {
        // #99 PR11 — pre-flight credit must exactly match the
        // number of files that will actually hit on warm lookup.
        // Drift rows and missing rows must NOT count, otherwise
        // the bar over-credits and freezes (PR7's failure mode).
        let p = tmp_db();
        let mut cache = Cache::open(&p).unwrap();
        let stored = [
            (
                key(1, 1024, 100_000, 7),
                CachedHashes {
                    tier3_hash: Some(vec![1; 32]),
                    ..CachedHashes::default()
                },
            ),
            (
                key(2, 2048, 200_000, 8),
                CachedHashes {
                    tier3_hash: Some(vec![2; 32]),
                    ..CachedHashes::default()
                },
            ),
            (
                key(3, 4096, 300_000, 9),
                CachedHashes {
                    tier3_hash: Some(vec![3; 32]),
                    ..CachedHashes::default()
                },
            ),
        ];
        for (k, h) in &stored {
            cache.store(k, h).unwrap();
        }
        cache.warm_in_place(HashAlgo::Blake3).unwrap();

        let preflight = [
            key(1, 1024, 100_000, 7), // exact → Hit
            key(2, 9999, 200_000, 8), // size drift → not counted
            key(3, 4096, 300_000, 9), // exact → Hit
            key(99, 1, 1, 1),         // no row → not counted
        ];
        assert_eq!(cache.predict_hits(&preflight), 2);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn predict_hits_is_zero_when_warm_map_absent() {
        // If warm_in_place wasn't called (e.g. cold cache or
        // schema mismatch), predict_hits returns 0 and the bar
        // falls back to the climb-during-fast-forward UX.
        let p = tmp_db();
        let cache = Cache::open(&p).unwrap();
        let k = key(1, 1024, 100_000, 7);
        cache.store(&k, &CachedHashes::default()).unwrap();
        assert_eq!(cache.predict_hits(&[k]), 0);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn volume_usn_roundtrip() {
        let p = tmp_db();
        let cache = Cache::open(&p).unwrap();
        cache.set_volume_usn("vol:abc", 12345).unwrap();
        assert_eq!(cache.get_volume_usn("vol:abc").unwrap(), Some(12345));
        cache.set_volume_usn("vol:abc", 67890).unwrap();
        assert_eq!(cache.get_volume_usn("vol:abc").unwrap(), Some(67890));
        assert_eq!(cache.get_volume_usn("vol:nope").unwrap(), None);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn clear_wipes_rows() {
        let p = tmp_db();
        let cache = Cache::open(&p).unwrap();
        for i in 0..10 {
            cache
                .store(&key(i, 100, 1, 1), &CachedHashes::default())
                .unwrap();
        }
        cache.clear().unwrap();
        assert!(cache.lookup(&key(5, 100, 1, 1)).unwrap().is_none());
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn store_merges_per_tier_writes_instead_of_replacing() {
        // Regression for the Pause → Resume hashing bug: the hash
        // pipeline writes one tier at a time, each call carrying only
        // that tier populated in CachedHashes. The earlier `store`
        // SQL set every tier column to `excluded.X`, so the third
        // write erased the first two. On resume, only the deepest
        // tier survived and Tiers 1–2 re-ran from cold even though
        // their results had been computed and (supposedly) cached.
        let p = tmp_db();
        let cache = Cache::open(&p).unwrap();
        let k = key(42, 100, 1, 1);

        // Tier 1 fires first.
        cache
            .store(
                &k,
                &CachedHashes {
                    tier1_hash: Some(b"T1".to_vec()),
                    ..Default::default()
                },
            )
            .unwrap();
        // Tier 2 next — only tier2 populated.
        cache
            .store(
                &k,
                &CachedHashes {
                    tier2_hash: Some(b"T2".to_vec()),
                    ..Default::default()
                },
            )
            .unwrap();
        // Tier 3 last — only tier3 populated.
        cache
            .store(
                &k,
                &CachedHashes {
                    tier3_hash: Some(b"T3".to_vec()),
                    ..Default::default()
                },
            )
            .unwrap();

        let loaded = cache.lookup(&k).unwrap().expect("row present");
        assert_eq!(loaded.tier1_hash.as_deref(), Some(b"T1".as_ref()), "tier1");
        assert_eq!(loaded.tier2_hash.as_deref(), Some(b"T2".as_ref()), "tier2");
        assert_eq!(loaded.tier3_hash.as_deref(), Some(b"T3".as_ref()), "tier3");
        std::fs::remove_file(&p).ok();
    }

    /// v7 schema: reparse_tag persists across save/load so the warm
    /// path can pass it back to `placeholder::classify()` and get the
    /// same PlaceholderState the cold path computed at scan time.
    #[test]
    fn inventory_snapshot_reparse_tag_roundtrips() {
        let p = tmp_db();
        let mut cache = Cache::open(&p).unwrap();
        let vol = "vol-test";
        let meta = InventoryMeta {
            journal_id: 1,
            last_usn: 100,
            captured_at_unix: now_unix(),
        };
        // Three records covering the cases that matter:
        // * No reparse point — reparse_tag = None
        // * Reparse with known dedup tag (0x80000013) — should
        //   roundtrip exactly
        // * Reparse with unknown tag — also roundtrips exactly so
        //   future analyses can audit what was actually seen
        let records = vec![
            (
                10u64,
                InventoryRecord {
                    parent_ref: 1,
                    usn: 10,
                    attributes: 0x80, // FILE_ATTRIBUTE_NORMAL-ish, no reparse
                    name: "plain.txt".into(),
                    size: 100,
                    mtime: 0,
                    reparse_tag: None,
                },
            ),
            (
                20u64,
                InventoryRecord {
                    parent_ref: 1,
                    usn: 20,
                    attributes: 0x400, // FILE_ATTRIBUTE_REPARSE_POINT
                    name: "dedup.dat".into(),
                    size: 1024,
                    mtime: 0,
                    reparse_tag: Some(0x8000_0013),
                },
            ),
            (
                30u64,
                InventoryRecord {
                    parent_ref: 1,
                    usn: 30,
                    attributes: 0x400,
                    name: "unknown-reparse".into(),
                    size: 2048,
                    mtime: 0,
                    reparse_tag: Some(0xDEAD_BEEF),
                },
            ),
        ];
        cache.save_inventory_snapshot(vol, &meta, &records).unwrap();

        let loaded = cache.load_inventory_records(vol).unwrap();
        assert_eq!(loaded.len(), 3);
        // Order isn't guaranteed by load_inventory_records — index by file_ref.
        let by_ref: std::collections::HashMap<u64, &InventoryRecord> =
            loaded.iter().map(|(r, rec)| (*r, rec)).collect();
        assert_eq!(by_ref[&10].reparse_tag, None, "plain file");
        assert_eq!(
            by_ref[&20].reparse_tag,
            Some(0x8000_0013),
            "dedup reparse tag roundtrips"
        );
        assert_eq!(
            by_ref[&30].reparse_tag,
            Some(0xDEAD_BEEF),
            "unknown reparse tag roundtrips for future audit"
        );
        std::fs::remove_file(&p).ok();
    }
}
