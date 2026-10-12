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

/// Returns every scan in the order in which the scans started.
pub fn list(conn: &Connection) -> Result<Vec<Scan>> {
    let mut select = conn.prepare(
        "SELECT id, started_at, finished_at, tracks_seen, tracks_added, tracks_changed,
           tracks_moved, tracks_removed, error
         FROM scans ORDER BY id",
    )?;
    let scans = select
        .query_map([], scan_from_row)?
        .collect::<rusqlite::Result<Vec<Scan>>>()?;
    Ok(scans)
}

/// Returns the end time of the scan that ended last, or `None` when no scan
/// has ended. The time is ISO-8601 in UTC with seconds, as in
/// `2026-10-11T08:15:30Z`.
///
/// A scan that ended with an error counts, and so does a cancelled scan. A
/// scan that runs has no end time. A scan whose process ended before the
/// scan finished has none either.
///
/// SQLite formats the time, so the crate holds no date arithmetic for it.
pub fn last_finished(conn: &Connection) -> Result<Option<String>> {
    // An aggregate without `GROUP BY` returns one row, with NULL when no row
    // has an end time.
    let time = conn.query_row(
        "SELECT strftime('%Y-%m-%dT%H:%M:%SZ', max(finished_at), 'unixepoch') FROM scans",
        [],
        |row| row.get(0),
    )?;
    Ok(time)
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
    fn list_returns_every_scan_in_the_order_of_the_starts() {
        let db = Db::open_memory().unwrap();
        assert_eq!(db.with_reader(list).unwrap(), []);

        let first = db.with_writer(|conn| insert(conn)).unwrap();
        let second = db.with_writer(|conn| insert(conn)).unwrap();
        db.with_writer(|conn| finish(conn, second, &COUNTS, Some("cancelled")))
            .unwrap();

        let scans = db.with_reader(list).unwrap();
        let ids: Vec<i64> = scans.iter().map(|scan| scan.id).collect();
        assert_eq!(ids, [first, second]);
        assert_eq!(scans[0].finished_at, None);
        assert_eq!(scans[0].counts, ScanCounts::default());
        assert!(scans[1].finished_at.is_some());
        assert_eq!(scans[1].counts, COUNTS);
        assert_eq!(scans[1].error.as_deref(), Some("cancelled"));
        assert_eq!(db.with_reader(latest).unwrap().as_ref(), scans.last());
    }

    /// Sets the end time of the scan `id` to `unix_seconds`.
    fn set_finished_at(db: &Db, id: i64, unix_seconds: i64) {
        db.with_writer(|conn| {
            conn.execute(
                "UPDATE scans SET finished_at = ?2 WHERE id = ?1",
                params![id, unix_seconds],
            )?;
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn last_finished_is_none_until_a_scan_has_ended() {
        let db = Db::open_memory().unwrap();
        assert_eq!(db.with_reader(last_finished).unwrap(), None);

        // A scan that runs has no end time.
        db.with_writer(|conn| insert(conn)).unwrap();
        assert_eq!(db.with_reader(last_finished).unwrap(), None);
    }

    #[test]
    fn last_finished_is_the_latest_end_time_in_utc() {
        /// 2026-10-11T00:00:00Z.
        const DAY: i64 = 1_791_676_800;

        let db = Db::open_memory().unwrap();
        let first = db.with_writer(|conn| insert(conn)).unwrap();
        let second = db.with_writer(|conn| insert(conn)).unwrap();
        let third = db.with_writer(|conn| insert(conn)).unwrap();

        set_finished_at(&db, first, DAY + 3_723);
        assert_eq!(
            db.with_reader(last_finished).unwrap().as_deref(),
            Some("2026-10-11T01:02:03Z")
        );

        // The scan that started last ended first. The other end time is the
        // latest. The third scan still runs.
        set_finished_at(&db, second, DAY + 59);
        assert_eq!(
            db.with_reader(last_finished).unwrap().as_deref(),
            Some("2026-10-11T01:02:03Z")
        );
        set_finished_at(&db, second, DAY + 86_399);
        assert_eq!(
            db.with_reader(last_finished).unwrap().as_deref(),
            Some("2026-10-11T23:59:59Z")
        );
        assert_eq!(db.with_reader(latest).unwrap().unwrap().id, third);
    }

    #[test]
    fn last_finished_counts_a_scan_that_ended_with_an_error() {
        let db = Db::open_memory().unwrap();
        let id = db.with_writer(|conn| insert(conn)).unwrap();
        db.with_writer(|conn| finish(conn, id, &COUNTS, Some("cancelled")))
            .unwrap();
        set_finished_at(&db, id, 0);
        assert_eq!(
            db.with_reader(last_finished).unwrap().as_deref(),
            Some("1970-01-01T00:00:00Z")
        );
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
