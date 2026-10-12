//! The queries of `getIndexes` on the `scans` table (`docs/design.md`
//! section 4): the time at which the artists of the library last changed,
//! and whether a scan has no end time.
//!
//! # What changes the artists
//!
//! The artists that `getIndexes` lists follow from the track rows: an artist
//! row exists while a track refers to it, its name is a spelling that its
//! tracks carry, and it is in a music folder while one of its tracks has a
//! file there. Only a scan writes track rows. [`last_modified`] is therefore
//! the end time of the last scan that can have written one.
//!
//! A scan that has ended counts as such a scan unless all of this holds:
//!
//! - It stored no change of a track: `tracks_added`, `tracks_changed`,
//!   `tracks_moved` and `tracks_removed` are 0. Every track row that a scan
//!   inserts, updates, relinks or deletes adds to one of the four.
//! - It recorded no error. A scan that takes the file from a track row
//!   writes that row and counts it nowhere, and it records an error that
//!   names the track. A cancelled scan and a scan that a database error
//!   ended have an error too.
//! - The scan before it ended without an error. Otherwise that scan can have
//!   left work for this one: a cancelled scan deletes no artist row, and a
//!   scan whose process ended has no end time for the rows it stored. The
//!   first scan of a database has no scan before it and counts.
//!
//! A scan for which the three hold found the track rows as a completed scan
//! left them and wrote none, so its last step deleted no artist row and
//! changed no name. It can have set the cover of an album, which no artist
//! of `getIndexes` shows.
//!
//! # Milliseconds
//!
//! `scans.finished_at` holds whole seconds. Two scans can end in the same
//! second, and a client that read the artists after the first of them must
//! be told of the second. The value of a scan is therefore its end time in
//! milliseconds plus the number of scans before it that ended in the same
//! second. Scans run one at a time, so their IDs are in the order of their
//! end times, and the value of a later scan is greater while the clock of
//! the host does not go back.

use rusqlite::{Connection, OptionalExtension};

use crate::db::Result;

/// The most scans of one second that [`last_modified`] tells apart. The
/// statement looks at this many rows before a scan for scans that ended in
/// the same second, so the milliseconds that it adds stay below one second.
const SCANS_PER_SECOND: i64 = 1000;

/// Returns the SQL text of [`last_modified`].
///
/// The statement reads the scans from the last one backwards, by the row ID,
/// and stops at the first that counts (see the module comment). Its two
/// subqueries read rows by their ID: the scan before the one that is tested,
/// and the at most [`SCANS_PER_SECOND`] scans before the one that is found.
fn last_modified_sql() -> String {
    format!(
        "SELECT scans.finished_at,
           (SELECT count(*) FROM scans AS earlier
            WHERE earlier.id < scans.id AND earlier.id > scans.id - {SCANS_PER_SECOND}
              AND earlier.finished_at = scans.finished_at)
         FROM scans
         WHERE scans.finished_at IS NOT NULL
           AND (scans.tracks_added > 0 OR scans.tracks_changed > 0
                OR scans.tracks_moved > 0 OR scans.tracks_removed > 0
                OR scans.error IS NOT NULL
                OR NOT coalesce(
                     (SELECT previous.finished_at IS NOT NULL AND previous.error IS NULL
                      FROM scans AS previous
                      WHERE previous.id < scans.id
                      ORDER BY previous.id DESC LIMIT 1),
                     0))
         ORDER BY scans.id DESC
         LIMIT 1"
    )
}

/// Returns the time at which the artists of the library last changed, in
/// milliseconds since the Unix epoch, or `None` when no scan has ended.
/// `getIndexes` reports it as `lastModified`.
///
/// The value is the end time of the last scan that can have changed a track
/// row (see the module comment). It is the same before and after a scan that
/// found nothing changed, and it is greater after a scan that stored a
/// change, also when both scans ended in the same second.
pub fn last_modified(conn: &Connection) -> Result<Option<i64>> {
    let found: Option<(i64, i64)> = conn
        .prepare_cached(&last_modified_sql())?
        .query_row([], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()?;
    Ok(found.map(|(finished_at, earlier)| {
        // The end time in milliseconds, and one millisecond for each scan
        // before this one that ended in the same second.
        finished_at.saturating_mul(1000).saturating_add(earlier)
    }))
}

/// Returns whether the scan that started last has no end time: it runs, or
/// its process ended before the scan did. Returns `false` when no scan has
/// started.
///
/// Such a scan can have stored changes that the value of [`last_modified`]
/// does not cover yet, so `getIndexes` returns the artists to every request
/// while this is `true`.
pub fn scan_is_open(conn: &Connection) -> Result<bool> {
    let open: Option<bool> = conn
        .prepare_cached("SELECT finished_at IS NULL FROM scans ORDER BY id DESC LIMIT 1")?
        .query_row([], |row| row.get(0))
        .optional()?;
    Ok(open.unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use rusqlite::params;

    use super::*;
    use crate::db::Db;
    use crate::db::queries::scans::{self, ScanCounts};

    /// 2026-10-11T00:00:00Z.
    const DAY: i64 = 1_791_676_800;

    /// The counts of a scan that stored no change of a track.
    const UNCHANGED: ScanCounts = ScanCounts {
        seen: 10,
        added: 0,
        changed: 0,
        moved: 0,
        removed: 0,
    };

    /// The counts of a scan that inserted one track row.
    const ADDED: ScanCounts = ScanCounts {
        added: 1,
        ..UNCHANGED
    };

    fn db() -> Db {
        Db::open_memory().unwrap()
    }

    fn read(db: &Db) -> Option<i64> {
        db.with_reader(last_modified).unwrap()
    }

    /// Starts a scan and returns its ID. The scan has no end time.
    fn start(db: &Db) -> i64 {
        db.with_writer(|conn| scans::insert(conn)).unwrap()
    }

    /// Ends the scan `id` at `finished_at` with `counts` and `error`.
    fn end(db: &Db, id: i64, finished_at: i64, counts: ScanCounts, error: Option<&str>) {
        db.with_writer(|conn| {
            scans::finish(conn, id, &counts, error)?;
            conn.execute(
                "UPDATE scans SET finished_at = ?2 WHERE id = ?1",
                params![id, finished_at],
            )?;
            Ok(())
        })
        .unwrap();
    }

    /// Runs a scan that ends at `finished_at` with `counts` and `error`.
    fn scan(db: &Db, finished_at: i64, counts: ScanCounts, error: Option<&str>) -> i64 {
        let id = start(db);
        end(db, id, finished_at, counts, error);
        id
    }

    #[test]
    fn last_modified_is_none_until_a_scan_has_ended() {
        let db = db();
        assert_eq!(read(&db), None);

        // A scan that runs has no end time.
        let id = start(&db);
        assert_eq!(read(&db), None);

        end(&db, id, DAY, ADDED, None);
        assert_eq!(read(&db), Some(DAY * 1000));
    }

    #[test]
    fn last_modified_is_the_end_time_in_milliseconds() {
        let db = db();
        scan(&db, DAY + 3_723, ADDED, None);
        assert_eq!(read(&db), Some(1_791_680_523_000));
    }

    #[test]
    fn the_first_scan_counts_also_when_it_stored_no_change() {
        let db = db();
        scan(&db, DAY, UNCHANGED, None);
        assert_eq!(read(&db), Some(DAY * 1000));
    }

    #[test]
    fn a_scan_that_stored_a_change_of_a_track_counts() {
        let changes = [
            ScanCounts {
                added: 1,
                ..UNCHANGED
            },
            ScanCounts {
                changed: 1,
                ..UNCHANGED
            },
            ScanCounts {
                moved: 1,
                ..UNCHANGED
            },
            ScanCounts {
                removed: 1,
                ..UNCHANGED
            },
        ];
        for counts in changes {
            let db = db();
            scan(&db, DAY, ADDED, None);
            scan(&db, DAY + 60, counts, None);
            assert_eq!(read(&db), Some((DAY + 60) * 1000), "{counts:?}");
        }
    }

    #[test]
    fn a_scan_that_found_nothing_changed_keeps_the_value() {
        let db = db();
        scan(&db, DAY, ADDED, None);
        let before = read(&db);
        assert_eq!(before, Some(DAY * 1000));

        // The files it saw are no change.
        for minute in 1..=3 {
            scan(&db, DAY + 60 * minute, UNCHANGED, None);
            assert_eq!(read(&db), before, "{minute}");
        }

        // The next scan with a change counts again.
        scan(&db, DAY + 600, ADDED, None);
        assert_eq!(read(&db), Some((DAY + 600) * 1000));
        scan(&db, DAY + 660, UNCHANGED, None);
        assert_eq!(read(&db), Some((DAY + 600) * 1000));
    }

    #[test]
    fn a_scan_with_an_error_counts_and_so_does_the_scan_after_it() {
        for error in [
            "cancelled",
            "database is locked",
            "1 track whose file is gone was kept, because a path or a file could not be read",
        ] {
            let db = db();
            scan(&db, DAY, ADDED, None);
            // The scan stored no change, and it has an error.
            scan(&db, DAY + 60, UNCHANGED, Some(error));
            assert_eq!(read(&db), Some((DAY + 60) * 1000), "{error}");
            // The scan after it does what it left: it counts without a
            // change and without an error.
            scan(&db, DAY + 120, UNCHANGED, None);
            assert_eq!(read(&db), Some((DAY + 120) * 1000), "{error}");
            // The scan after a scan that completed does not.
            scan(&db, DAY + 180, UNCHANGED, None);
            assert_eq!(read(&db), Some((DAY + 120) * 1000), "{error}");
        }
    }

    #[test]
    fn the_scan_after_a_scan_without_an_end_time_counts() {
        let db = db();
        scan(&db, DAY, ADDED, None);
        // The process of this scan ended before the scan did.
        start(&db);
        assert_eq!(read(&db), Some(DAY * 1000));

        scan(&db, DAY + 60, UNCHANGED, None);
        assert_eq!(read(&db), Some((DAY + 60) * 1000));
        scan(&db, DAY + 120, UNCHANGED, None);
        assert_eq!(read(&db), Some((DAY + 60) * 1000));
    }

    #[test]
    fn a_scan_that_runs_does_not_hide_the_scans_that_ended() {
        let db = db();
        scan(&db, DAY, ADDED, None);
        scan(&db, DAY + 60, UNCHANGED, None);
        start(&db);
        assert_eq!(read(&db), Some(DAY * 1000));
    }

    #[test]
    fn scans_that_end_in_the_same_second_have_values_in_their_order() {
        let db = db();
        scan(&db, DAY, ADDED, None);
        let first = read(&db).unwrap();
        assert_eq!(first, DAY * 1000);

        scan(&db, DAY, ADDED, None);
        let second = read(&db).unwrap();
        assert_eq!(second, DAY * 1000 + 1);

        // A scan without a change in the same second keeps the value, and
        // the scan after it has a greater one.
        scan(&db, DAY, UNCHANGED, None);
        assert_eq!(read(&db), Some(second));
        scan(&db, DAY, ADDED, None);
        let fourth = read(&db).unwrap();
        assert_eq!(fourth, DAY * 1000 + 3);

        // The first scan of the next second has a greater value than every
        // scan of this one.
        scan(&db, DAY + 1, ADDED, None);
        assert_eq!(read(&db), Some((DAY + 1) * 1000));
        assert!(first < second && second < fourth && fourth < (DAY + 1) * 1000);
    }

    #[test]
    fn the_milliseconds_of_one_second_stay_below_the_next_second() {
        let db = db();
        let mut last = None;
        for _ in 0..SCANS_PER_SECOND + 5 {
            scan(&db, DAY, ADDED, None);
            let value = read(&db).unwrap();
            assert!((DAY * 1000..(DAY + 1) * 1000).contains(&value), "{value}");
            assert!(last <= Some(value), "{last:?} {value}");
            last = Some(value);
        }
        assert_eq!(last, Some(DAY * 1000 + SCANS_PER_SECOND - 1));
    }

    #[test]
    fn scan_is_open_while_the_scan_that_started_last_has_no_end_time() {
        let db = db();
        let open = |db: &Db| db.with_reader(scan_is_open).unwrap();
        assert!(!open(&db));

        let first = start(&db);
        assert!(open(&db));
        end(&db, first, DAY, ADDED, None);
        assert!(!open(&db));

        // A scan without an end time that a later scan follows is not the
        // scan that started last.
        start(&db);
        assert!(open(&db));
        let third = start(&db);
        assert!(open(&db));
        end(&db, third, DAY + 60, UNCHANGED, Some("cancelled"));
        assert!(!open(&db));
    }

    #[test]
    fn last_modified_reads_the_scans_backwards_and_sorts_nothing() {
        let db = db();
        let plan: Vec<String> = db
            .with_reader(|conn| {
                let rows = conn
                    .prepare(&format!("EXPLAIN QUERY PLAN {}", last_modified_sql()))?
                    .query_map([], |row| row.get(3))?
                    .collect::<rusqlite::Result<_>>()?;
                Ok(rows)
            })
            .unwrap();
        // The scans are read in the order of the row ID, so the statement
        // stops at the first scan that counts.
        assert!(plan.contains(&"SCAN scans".to_owned()), "{plan:?}");
        assert!(
            plan.iter().all(|step| !step.contains("TEMP B-TREE")),
            "{plan:?}"
        );
        // The other reads of the table find their rows by the row ID.
        for alias in ["earlier", "previous"] {
            assert!(
                plan.iter().any(|step| step
                    .starts_with(&format!("SEARCH {alias} USING INTEGER PRIMARY KEY (rowid"))),
                "{alias}: {plan:?}"
            );
        }
        assert_eq!(
            plan.iter().filter(|step| step.starts_with("SCAN ")).count(),
            1,
            "{plan:?}"
        );
    }
}
