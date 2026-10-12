//! Queries on the `scans` table. A row is one run of the scanner.

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::int;
use crate::db::Result;

/// The track counts of one scan.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanCounts {
    /// The supported files the scan saw.
    pub seen: u64,
    /// The rows the scan inserted.
    pub added: u64,
    /// The rows the scan updated in place.
    pub changed: u64,
    /// The rows the scan gave a new folder or path.
    pub moved: u64,
    /// The rows the scan deleted.
    pub removed: u64,
}

/// A row of the `scans` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scan {
    pub id: i64,
    /// The start time in Unix seconds.
    pub started_at: i64,
    /// The end time in Unix seconds. `None` while the scan runs, and for a
    /// scan whose process ended before it finished.
    pub finished_at: Option<i64>,
    pub counts: ScanCounts,
    /// What kept the scan from completing or from deleting tracks. `None`
    /// for a scan without a problem.
    pub error: Option<String>,
}

/// Inserts a row for a scan that starts now and returns its ID.
pub fn insert(conn: &Connection) -> Result<i64> {
    conn.execute("INSERT INTO scans (started_at) VALUES (unixepoch())", [])?;
    Ok(conn.last_insert_rowid())
}

/// Stores the end time, which is now, the counts and the error of the scan
/// `id`.
pub fn finish(conn: &Connection, id: i64, counts: &ScanCounts, error: Option<&str>) -> Result<()> {
    conn.execute(
        "UPDATE scans SET finished_at = unixepoch(), tracks_seen = ?2, tracks_added = ?3,
           tracks_changed = ?4, tracks_moved = ?5, tracks_removed = ?6, error = ?7
         WHERE id = ?1",
        params![
            id,
            int::to_sql(counts.seen)?,
            int::to_sql(counts.added)?,
            int::to_sql(counts.changed)?,
            int::to_sql(counts.moved)?,
            int::to_sql(counts.removed)?,
            error
        ],
    )?;
    Ok(())
}

/// Returns the scan that started last, or `None` when no scan has run.
pub fn latest(conn: &Connection) -> Result<Option<Scan>> {
    let scan = conn
        .query_row(
            "SELECT id, started_at, finished_at, tracks_seen, tracks_added, tracks_changed,
               tracks_moved, tracks_removed, error
             FROM scans ORDER BY id DESC LIMIT 1",
            [],
            scan_from_row,
        )
        .optional()?;
    Ok(scan)
}

fn scan_from_row(row: &Row<'_>) -> rusqlite::Result<Scan> {
    Ok(Scan {
        id: row.get(0)?,
        started_at: row.get(1)?,
        finished_at: row.get(2)?,
        counts: ScanCounts {
            seen: int::get_u64(row, 3)?,
            added: int::get_u64(row, 4)?,
            changed: int::get_u64(row, 5)?,
            moved: int::get_u64(row, 6)?,
            removed: int::get_u64(row, 7)?,
        },
        error: row.get(8)?,
    })
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::db::Db;

    const COUNTS: ScanCounts = ScanCounts {
        seen: 10,
        added: 4,
        changed: 3,
        moved: 2,
        removed: 1,
    };

    fn unix_now() -> i64 {
        let elapsed = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        i64::try_from(elapsed.as_secs()).unwrap()
    }

    #[test]
    fn latest_is_none_before_the_first_scan() {
        let db = Db::open_memory().unwrap();
        assert_eq!(db.with_reader(latest).unwrap(), None);
    }

    #[test]
    fn insert_starts_a_scan_that_has_not_finished() {
        let db = Db::open_memory().unwrap();
        let before = unix_now();
        let id = db.with_writer(|conn| insert(conn)).unwrap();
        let after = unix_now();

        let scan = db.with_reader(latest).unwrap().unwrap();
        assert_eq!(scan.id, id);
        assert!((before..=after).contains(&scan.started_at), "{scan:?}");
        assert_eq!(scan.finished_at, None);
        assert_eq!(scan.counts, ScanCounts::default());
        assert_eq!(scan.error, None);
    }

    #[test]
    fn finish_stores_the_end_time_the_counts_and_the_error() {
        let db = Db::open_memory().unwrap();
        let id = db.with_writer(|conn| insert(conn)).unwrap();
        let before = unix_now();
        db.with_writer(|conn| finish(conn, id, &COUNTS, Some("cancelled")))
            .unwrap();
        let after = unix_now();

        let scan = db.with_reader(latest).unwrap().unwrap();
        assert_eq!(scan.id, id);
        let finished_at = scan.finished_at.unwrap();
        assert!((before..=after).contains(&finished_at), "{scan:?}");
        assert_eq!(scan.counts, COUNTS);
        assert_eq!(scan.error.as_deref(), Some("cancelled"));
    }

    #[test]
    fn finish_without_an_error_stores_null() {
        let db = Db::open_memory().unwrap();
        let id = db.with_writer(|conn| insert(conn)).unwrap();
        db.with_writer(|conn| finish(conn, id, &COUNTS, None))
            .unwrap();
        let scan = db.with_reader(latest).unwrap().unwrap();
        assert!(scan.finished_at.is_some());
        assert_eq!(scan.error, None);
    }

    #[test]
    fn latest_returns_the_scan_that_started_last() {
        let db = Db::open_memory().unwrap();
        let first = db.with_writer(|conn| insert(conn)).unwrap();
        let second = db.with_writer(|conn| insert(conn)).unwrap();
        db.with_writer(|conn| finish(conn, first, &COUNTS, None))
            .unwrap();

        let scan = db.with_reader(latest).unwrap().unwrap();
        assert_ne!(first, second);
        assert_eq!(scan.id, second);
        assert_eq!(scan.finished_at, None);
    }
}
