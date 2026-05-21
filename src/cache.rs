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
/// * v3 is the same shape as v2 but bumps the version string so v2
///   caches — which tagged the 128-bit rows as `"ddh128"` — get
///   discarded after the crate rename to `river128`. Old rows are
///   functionally orphaned (new lookups use the new tag), and the
///   cache is cheap to rebuild.
///
/// Bumping this string causes init_schema to drop and recreate the
/// tables; any cached data from older versions is discarded.
const SCHEMA_VERSION: &str = "3";

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
                // Schema mismatch — drop and recreate.
                self.conn
                    .execute_batch("DROP TABLE IF EXISTS files; DROP TABLE IF EXISTS volumes;")?;
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
            CREATE INDEX IF NOT EXISTS idx_tier3 ON files(tier3_hash) WHERE tier3_hash IS NOT NULL;",
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
                hashes.tier0_fingerprint.as_ref().map(|h| h.as_slice()),
                hashes.tier1_hash.as_ref().map(|h| h.as_slice()),
                hashes.tier2_hash.as_ref().map(|h| h.as_slice()),
                hashes.tier3_hash.as_ref().map(|h| h.as_slice()),
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
