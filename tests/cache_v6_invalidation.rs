//! T2.1 criterion #9 — cache schema v6 → v7 invalidation.
//!
//! Opening a v6-shaped sqlite via `Cache::open` must drop all owned
//! tables and recreate them under v7. No row carries over (this is
//! invalidation-rebuild, not in-place migration), and the resulting
//! `inventory_records` table includes the `reparse_tag` column added
//! in v7.

use std::path::PathBuf;

use rusqlite::Connection;
use tempfile::tempdir;

#[test]
fn v6_cache_is_invalidated_to_v7_empty() {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v6_cache.sqlite");
    let tmp = tempdir().unwrap();
    let dst = tmp.path().join("v6_to_v7.sqlite");
    std::fs::copy(&fixture, &dst).unwrap();

    // Open via the public Cache API — silently detects v6 in `meta`,
    // drops tables, recreates as v7. Scope the connection so it's
    // dropped before we reopen with rusqlite.
    {
        let _cache = superdeduper::cache::Cache::open(&dst).unwrap();
    }

    let conn = Connection::open(&dst).unwrap();

    let version: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(version, "7", "schema_version should be bumped to 7");

    let mut stmt = conn
        .prepare("PRAGMA table_info(inventory_records)")
        .unwrap();
    let cols: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert!(
        cols.iter().any(|c| c == "reparse_tag"),
        "inventory_records should have a reparse_tag column at v7, got {:?}",
        cols
    );

    for table in ["files", "volumes", "inventory_meta", "inventory_records"] {
        let count: i64 = conn
            .query_row(&format!("SELECT count(*) FROM {}", table), [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            count, 0,
            "{} should be empty after v6→v7 invalidation, got {} rows",
            table, count
        );
    }
}
