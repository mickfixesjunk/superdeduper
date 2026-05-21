//! Per-machine persistent cache, keyed by `(volume_guid, file_ref)` and
//! invalidated whenever `(size, mtime, usn)` changes for a record.
//!
//! The cache lives at `%LOCALAPPDATA%\superdupe\cache.db` on Windows
//! and `$XDG_CACHE_HOME/superdupe/cache.db` (or `~/.cache/superdupe/`)
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
/// * v5 adds the inventory-snapshot tables (`inventory_meta` +
///   `inventory_records`) so the warm-path Stage 1 enumerator can
///   apply a USN-journal delta against a cached baseline instead
///   of re-walking the whole MFT. Drops the v4 hash rows in the
///   process, which is fine — they re-populate on next scan.
///
/// Bumping this string causes init_schema to drop and recreate the
/// tables; any cached data from older versions is discarded.
const SCHEMA_VERSION: &str = "5";

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
}

#[derive(Debug, Clone)]
pub struct CacheStats {
    pub path: PathBuf,
    pub rows: u64,
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

        let cache = Cache { conn };
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
            CREATE TABLE IF NOT EXISTS inventory_records (
                volume_guid TEXT    NOT NULL,
                file_ref    INTEGER NOT NULL,
                parent_ref  INTEGER NOT NULL,
                usn         INTEGER NOT NULL,
                attributes  INTEGER NOT NULL,
                name        TEXT    NOT NULL,
                size        INTEGER NOT NULL,
                mtime       INTEGER NOT NULL,
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
        // Row layout from the SELECT below. Aliased so the function
        // signature isn't a 100-column horror.
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
            None => return Ok(None),
        };
        if size as u64 != key.size || mtime != key.mtime || usn != key.usn {
            return Ok(None);
        }

        Ok(Some(CachedHashes {
            tier0_fingerprint: t0,
            tier1_hash: t1,
            tier2_hash: t2,
            tier3_hash: t3,
        }))
    }

    pub fn store(&self, key: &CacheKey, hashes: &CachedHashes) -> Result<()> {
        let now = now_unix();
        self.conn.execute(
            "INSERT INTO files (
                volume_guid, file_ref, hash_algo, size, mtime, usn,
                tier0_fingerprint, tier1_hash, tier2_hash, tier3_hash, last_seen
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(volume_guid, file_ref, hash_algo) DO UPDATE SET
                size = excluded.size,
                mtime = excluded.mtime,
                usn = excluded.usn,
                tier0_fingerprint = excluded.tier0_fingerprint,
                tier1_hash = excluded.tier1_hash,
                tier2_hash = excluded.tier2_hash,
                tier3_hash = excluded.tier3_hash,
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
            "SELECT file_ref, parent_ref, usn, attributes, name, size, mtime
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
                     (volume_guid, file_ref, parent_ref, usn, attributes, name, size, mtime)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
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

    pub fn stats(&self, path: &Path) -> Result<CacheStats> {
        let rows: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?;
        let bytes_on_disk = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        Ok(CacheStats {
            path: path.to_path_buf(),
            rows: rows as u64,
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

#[cfg(windows)]
pub fn default_cache_path() -> Result<PathBuf> {
    let local = std::env::var("LOCALAPPDATA").map_err(|_| Error::other("LOCALAPPDATA not set"))?;
    Ok(PathBuf::from(local).join("superdupe").join("cache.db"))
}

#[cfg(not(windows))]
pub fn default_cache_path() -> Result<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(xdg).join("superdupe").join("cache.db"));
    }
    let home = std::env::var("HOME").map_err(|_| Error::other("HOME not set"))?;
    Ok(PathBuf::from(home)
        .join(".cache")
        .join("superdupe")
        .join("cache.db"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_db() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "superdupe-cache-{}-{}.db",
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
}
