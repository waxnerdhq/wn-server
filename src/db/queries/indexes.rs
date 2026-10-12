//! The queries of `getIndexes` on the `scans` table (`docs/design.md`
//! section 4): the time at which the artists of the library last changed,
//! and whether a scan has no end time.
//!
//! The artists that `getIndexes` lists follow from the rows of the library:
//! an artist row exists while a track refers to it, its name is a spelling
//! that its tracks carry, and it is in a music folder while one of its
//! tracks has a file there. Only a scan writes these rows. A scan knows
//! whether it wrote one, and it stores that in `scans.changed_library` when
//! it ends (`docs/design.md` section 3.10 step 7). [`last_modified`] is the
//! end time of the last scan whose flag is set.
//!
//! The end times are milliseconds, and each scan ends later than the scan
//! before it (`scans::finish`). The value of a later scan is therefore the
//! greater one, also when two scans end in one millisecond.

use rusqlite::{Connection, OptionalExtension};

use crate::db::Result;

/// The statement of [`last_modified`]. It reads the last entry of the index
/// `scans_changed` among the entries of the scans that changed the library,
/// whatever the number of scans.
const LAST_MODIFIED_SQL: &str = "SELECT max(finished_at) FROM scans WHERE changed_library = 1";

/// Returns the time at which the artists of the library last changed, in
/// milliseconds since the Unix epoch, or `None` when no scan that changed
/// the library has ended. `getIndexes` reports it as `lastModified`.
///
/// The value is the end time of the last scan that changed the library (see
/// the module comment). It is the same before and after a scan that changed
/// nothing, and it is greater after a scan that changed the library. A scan
/// without an end time does not count.
pub fn last_modified(conn: &Connection) -> Result<Option<i64>> {
    // An aggregate without `GROUP BY` returns one row, with NULL when no row
    // has the flag and an end time.
    let last_modified = conn
        .prepare_cached(LAST_MODIFIED_SQL)?
        .query_row([], |row| row.get(0))?;
    Ok(last_modified)
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
    use rusqlite::{StatementStatus, params};

    use super::*;
    use crate::db::Db;
    use crate::db::queries::scans::{self, ScanCounts};

    /// 2026-10-11T00:00:00Z in Unix milliseconds.
    const DAY: i64 = 1_791_676_800_000;

    /// One minute in milliseconds.
    const MINUTE: i64 = 60_000;

    /// The counts of a scan that saw ten files.
    const COUNTS: ScanCounts = ScanCounts {
        seen: 10,
        added: 0,
        changed: 0,
        moved: 0,
        removed: 0,
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

    /// Ends the scan `id` at `finished_at` with the flag `changed_library`.
    fn end(db: &Db, id: i64, finished_at: i64, changed_library: bool) {
        db.with_writer(|conn| {
            scans::finish(conn, id, &COUNTS, changed_library, None)?;
            conn.execute(
                "UPDATE scans SET finished_at = ?2 WHERE id = ?1",
                params![id, finished_at],
            )?;
            Ok(())
        })
        .unwrap();
    }

    /// Runs a scan that ends at `finished_at` with the flag
    /// `changed_library`.
    fn scan(db: &Db, finished_at: i64, changed_library: bool) -> i64 {
        let id = start(db);
        end(db, id, finished_at, changed_library);
        id
    }

    #[test]
    fn last_modified_is_none_until_a_scan_that_changed_the_library_has_ended() {
        let db = db();
        assert_eq!(read(&db), None);

        // A scan that runs has no end time.
        let id = start(&db);
        assert_eq!(read(&db), None);

        end(&db, id, DAY, true);
        assert_eq!(read(&db), Some(DAY));
    }

    #[test]
    fn last_modified_is_the_stored_end_time() {
        let db = db();
        // 2026-10-11T01:02:03.456Z.
        scan(&db, DAY + 3_723_456, true);
        assert_eq!(read(&db), Some(1_791_680_523_456));
    }

    #[test]
    fn the_flag_decides_whether_a_scan_counts() {
        let db = db();
        scan(&db, DAY, true);
        assert_eq!(read(&db), Some(DAY));

        // Scans that changed nothing keep the value.
        for minute in 1..=3 {
            scan(&db, DAY + MINUTE * minute, false);
            assert_eq!(read(&db), Some(DAY), "{minute}");
        }

        // The next scan that changed the library counts, and the scan
        // without a change after it does not.
        scan(&db, DAY + MINUTE * 10, true);
        assert_eq!(read(&db), Some(DAY + MINUTE * 10));
        scan(&db, DAY + MINUTE * 11, false);
        assert_eq!(read(&db), Some(DAY + MINUTE * 10));
    }

    #[test]
    fn the_counts_and_the_error_of_a_scan_do_not_decide() {
        let db = db();
        scan(&db, DAY, true);

        // A scan with a count of track changes and with an error, whose
        // flag is not set.
        let id = start(&db);
        db.with_writer(|conn| {
            let counts = ScanCounts {
                added: 1,
                changed: 1,
                moved: 1,
                removed: 1,
                ..COUNTS
            };
            scans::finish(conn, id, &counts, false, Some("cancelled"))
        })
        .unwrap();
        assert_eq!(read(&db), Some(DAY));

        // A scan without a count and without an error, whose flag is set.
        let id = scan(&db, DAY + MINUTE, true);
        let rows = db.with_reader(scans::list).unwrap();
        let row = rows.iter().find(|row| row.id == id).unwrap();
        assert_eq!((row.counts, row.error.as_deref()), (COUNTS, None));
        assert_eq!(read(&db), Some(DAY + MINUTE));
    }

    #[test]
    fn the_first_scan_counts_only_when_its_flag_is_set() {
        let unchanged = db();
        scan(&unchanged, DAY, false);
        assert_eq!(read(&unchanged), None);

        let changed = db();
        scan(&changed, DAY, true);
        assert_eq!(read(&changed), Some(DAY));
    }

    #[test]
    fn a_scan_without_an_end_time_does_not_count_and_hides_no_scan_that_ended() {
        let db = db();
        scan(&db, DAY, true);
        scan(&db, DAY + MINUTE, false);
        // The row of a scan that runs has the default of the flag, which
        // is 1, and no end time.
        start(&db);
        assert_eq!(read(&db), Some(DAY));
        assert!(db.with_reader(scan_is_open).unwrap());
    }

    #[test]
    fn the_value_is_the_latest_end_time_and_not_the_one_of_the_latest_row() {
        let db = db();
        // The clock of the host went back between two scans that the build
        // before schema version 3 stored.
        scan(&db, DAY + MINUTE, true);
        scan(&db, DAY, true);
        assert_eq!(read(&db), Some(DAY + MINUTE));
    }

    #[test]
    fn scans_that_end_one_after_the_other_have_values_in_their_order() {
        let db = db();
        let mut last = None;
        for _ in 0..50 {
            let id = start(&db);
            db.with_writer(|conn| scans::finish(conn, id, &COUNTS, true, None))
                .unwrap();
            let value = read(&db);
            assert!(last < value, "{last:?} {value:?}");
            last = value;

            // A scan without a change between two of them keeps the value.
            let id = start(&db);
            db.with_writer(|conn| scans::finish(conn, id, &COUNTS, false, None))
                .unwrap();
            assert_eq!(read(&db), last);
        }
    }

    #[test]
    fn scan_is_open_while_the_scan_that_started_last_has_no_end_time() {
        let db = db();
        let open = |db: &Db| db.with_reader(scan_is_open).unwrap();
        assert!(!open(&db));

        let first = start(&db);
        assert!(open(&db));
        end(&db, first, DAY, true);
        assert!(!open(&db));

        // A scan without an end time that a later scan follows is not the
        // scan that started last.
        start(&db);
        assert!(open(&db));
        let third = start(&db);
        assert!(open(&db));
        end(&db, third, DAY + MINUTE, false);
        assert!(!open(&db));
    }

    #[test]
    fn last_modified_reads_the_index_and_sorts_nothing() {
        let db = db();
        let plan: Vec<String> = db
            .with_reader(|conn| {
                let rows = conn
                    .prepare(&format!("EXPLAIN QUERY PLAN {LAST_MODIFIED_SQL}"))?
                    .query_map([], |row| row.get(3))?
                    .collect::<rusqlite::Result<_>>()?;
                Ok(rows)
            })
            .unwrap();
        assert_eq!(
            plan,
            ["SEARCH scans USING COVERING INDEX scans_changed (changed_library=?)"]
        );
    }

    /// Returns the number of steps that the virtual machine of SQLite takes
    /// for one run of the statement of [`last_modified`].
    fn steps(db: &Db) -> i32 {
        db.with_reader(|conn| {
            let mut statement = conn.prepare(LAST_MODIFIED_SQL)?;
            let value: Option<i64> = statement.query_row([], |row| row.get(0))?;
            assert!(value.is_some());
            Ok(statement.get_status(StatementStatus::VmStep))
        })
        .unwrap()
    }

    #[test]
    fn last_modified_does_the_same_work_for_any_number_of_scans() {
        let db = db();
        for minute in 0..3 {
            scan(&db, DAY + MINUTE * minute, true);
        }
        // A scan without an end time has the default of the flag, so its
        // entry is among those that the statement searches.
        start(&db);
        let few = steps(&db);

        // 10 000 more scans, half of them with the flag, and some without
        // an end time.
        db.with_writer(|conn| {
            conn.execute(
                "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 10000)
                 INSERT INTO scans (started_at, finished_at, changed_library)
                 SELECT ?1 + i, CASE WHEN i % 100 = 0 THEN NULL ELSE ?1 + i END, i % 2 FROM n",
                [DAY + MINUTE * 10],
            )?;
            Ok(())
        })
        .unwrap();
        assert_eq!(read(&db), Some(DAY + MINUTE * 10 + 9_999));
        assert_eq!(steps(&db), few);
    }
}
