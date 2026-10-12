//! Queries on the `scans` table. A row is one run of the scanner.
//!
//! The start time and the end time of a scan are Unix milliseconds.
//! [`finish`] gives a scan a later end time than every other row has. Two
//! rows have the same end time only when both were written before schema
//! version 3, which stored whole seconds, and ended in the same second.

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::int;
use crate::db::Result;

/// The present time in Unix milliseconds, as an SQL expression. SQLite
/// reads the clock once for a statement, so every use in one statement has
/// one value.
const NOW_MS_SQL: &str = "CAST(round(unixepoch('subsec') * 1000) AS INTEGER)";

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
    /// The start time in Unix milliseconds.
    pub started_at: i64,
    /// The end time in Unix milliseconds. `None` while the scan runs, and
    /// for a scan whose process ended before it finished.
    pub finished_at: Option<i64>,
    pub counts: ScanCounts,
    /// Whether the scan changed the library (`docs/design.md` section 3.10
    /// step 7). The scan stores the value when it ends. It is `true` in a
    /// row without an end time, and in a row that was written before schema
    /// version 3, which did not record what a scan wrote.
    pub changed_library: bool,
    /// What kept the scan from completing or from deleting tracks. `None`
    /// for a scan without a problem.
    pub error: Option<String>,
}

/// Inserts a row for a scan that starts now and returns its ID.
pub fn insert(conn: &Connection) -> Result<i64> {
    conn.execute(
        &format!("INSERT INTO scans (started_at) VALUES ({NOW_MS_SQL})"),
        [],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Stores the end time, the counts, whether the scan changed the library,
/// and the error of the scan `id`.
///
/// The end time is the present time, or the latest end time of the other
/// rows plus one millisecond when that is later. Scans run one at a time, so
/// the end times are strictly increasing in the order of the scans, also
/// when two scans end in one millisecond and when the clock of the host went
/// back. After the clock went back, the end times are ahead of the clock
/// until the clock has passed the latest of them.
///
/// The statement reads the end time of every other row, once for each scan.
pub fn finish(
    conn: &Connection,
    id: i64,
    counts: &ScanCounts,
    changed_library: bool,
    error: Option<&str>,
) -> Result<()> {
    conn.execute(
        &format!(
            "UPDATE scans SET
               finished_at = max({NOW_MS_SQL}, coalesce(
                 (SELECT max(other.finished_at) + 1 FROM scans AS other WHERE other.id <> ?1),
                 {NOW_MS_SQL})),
               tracks_seen = ?2, tracks_added = ?3, tracks_changed = ?4, tracks_moved = ?5,
               tracks_removed = ?6, changed_library = ?7, error = ?8
             WHERE id = ?1"
        ),
        params![
            id,
            int::to_sql(counts.seen)?,
            int::to_sql(counts.added)?,
            int::to_sql(counts.changed)?,
            int::to_sql(counts.moved)?,
            int::to_sql(counts.removed)?,
            changed_library,
            error
        ],
    )?;
    Ok(())
}

/// Returns whether the scan before the scan `id`, in the order of the IDs,
/// has no end time. Returns `false` when `id` is the first scan.
///
/// The process of such a scan ended before the scan did, or the scan could
/// not close its row. The rows that it stored are in the database, and no
/// row of the `scans` table says so.
pub fn previous_is_open(conn: &Connection, id: i64) -> Result<bool> {
    let open: Option<bool> = conn
        .query_row(
            "SELECT finished_at IS NULL FROM scans WHERE id < ?1 ORDER BY id DESC LIMIT 1",
            [id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(open.unwrap_or(false))
}

/// Returns the scan that started last, or `None` when no scan has run.
pub fn latest(conn: &Connection) -> Result<Option<Scan>> {
    let scan = conn
        .query_row(
            "SELECT id, started_at, finished_at, tracks_seen, tracks_added, tracks_changed,
               tracks_moved, tracks_removed, changed_library, error
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
           tracks_moved, tracks_removed, changed_library, error
         FROM scans ORDER BY id",
    )?;
    let scans = select
        .query_map([], scan_from_row)?
        .collect::<rusqlite::Result<Vec<Scan>>>()?;
    Ok(scans)
}

/// Returns the end time of the scan that ended last, or `None` when no scan
/// has ended. The time is ISO-8601 in UTC with seconds, as in
/// `2026-10-11T08:15:30Z`. The milliseconds of the stored time are cut off.
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
        "SELECT strftime('%Y-%m-%dT%H:%M:%SZ', max(finished_at) / 1000, 'unixepoch') FROM scans",
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
        changed_library: row.get(8)?,
        error: row.get(9)?,
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

    /// 2026-10-11T00:00:00Z in Unix seconds.
    const DAY: i64 = 1_791_676_800;

    /// The clock of the host in Unix milliseconds.
    fn now_ms() -> i64 {
        let elapsed = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        i64::try_from(elapsed.as_millis()).unwrap()
    }

    fn start(db: &Db) -> i64 {
        db.with_writer(|conn| insert(conn)).unwrap()
    }

    /// Ends the scan `id` with [`COUNTS`], a change of the library and no
    /// error, and returns its end time.
    fn end(db: &Db, id: i64) -> i64 {
        db.with_writer(|conn| finish(conn, id, &COUNTS, true, None))
            .unwrap();
        finished_at(db, id).expect("the scan has an end time")
    }

    fn finished_at(db: &Db, id: i64) -> Option<i64> {
        let scans = db.with_reader(list).unwrap();
        let scan = scans.iter().find(|scan| scan.id == id);
        scan.expect("the scan has a row").finished_at
    }

    /// Sets the end time of the scan `id` to `unix_ms`.
    fn set_finished_at(db: &Db, id: i64, unix_ms: i64) {
        db.with_writer(|conn| {
            conn.execute(
                "UPDATE scans SET finished_at = ?2 WHERE id = ?1",
                params![id, unix_ms],
            )?;
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn latest_is_none_before_the_first_scan() {
        let db = Db::open_memory().unwrap();
        assert_eq!(db.with_reader(latest).unwrap(), None);
    }

    #[test]
    fn insert_starts_a_scan_that_has_not_finished_at_a_time_in_milliseconds() {
        let db = Db::open_memory().unwrap();
        let before = now_ms();
        let id = start(&db);
        let after = now_ms();

        let scan = db.with_reader(latest).unwrap().unwrap();
        assert_eq!(scan.id, id);
        assert!((before..=after).contains(&scan.started_at), "{scan:?}");
        assert_eq!(scan.finished_at, None);
        assert_eq!(scan.counts, ScanCounts::default());
        // The default of the column, which a scan without an end time has.
        assert!(scan.changed_library);
        assert_eq!(scan.error, None);
    }

    #[test]
    fn finish_stores_the_end_time_in_milliseconds_the_counts_the_flag_and_the_error() {
        for changed_library in [false, true] {
            let db = Db::open_memory().unwrap();
            let id = start(&db);
            let before = now_ms();
            db.with_writer(|conn| finish(conn, id, &COUNTS, changed_library, Some("cancelled")))
                .unwrap();
            let after = now_ms();

            let scan = db.with_reader(latest).unwrap().unwrap();
            assert_eq!(scan.id, id);
            let finished_at = scan.finished_at.unwrap();
            assert!((before..=after).contains(&finished_at), "{scan:?}");
            assert!(scan.started_at <= finished_at, "{scan:?}");
            assert_eq!(scan.counts, COUNTS);
            assert_eq!(scan.changed_library, changed_library);
            assert_eq!(scan.error.as_deref(), Some("cancelled"));
        }
    }

    #[test]
    fn finish_without_an_error_stores_null() {
        let db = Db::open_memory().unwrap();
        let id = start(&db);
        db.with_writer(|conn| finish(conn, id, &COUNTS, false, None))
            .unwrap();
        let scan = db.with_reader(latest).unwrap().unwrap();
        assert!(scan.finished_at.is_some());
        assert_eq!(scan.error, None);
    }

    #[test]
    fn finish_writes_the_row_of_its_scan_only() {
        let db = Db::open_memory().unwrap();
        let first = start(&db);
        let second = start(&db);
        db.with_writer(|conn| finish(conn, second, &COUNTS, false, Some("cancelled")))
            .unwrap();

        let scans = db.with_reader(list).unwrap();
        assert_eq!(scans[0].id, first);
        assert_eq!(scans[0].finished_at, None);
        assert_eq!(scans[0].counts, ScanCounts::default());
        assert!(scans[0].changed_library);
        assert_eq!(scans[0].error, None);
    }

    #[test]
    fn scans_that_end_in_one_millisecond_have_end_times_in_their_order() {
        let db = Db::open_memory().unwrap();
        let before = now_ms();
        // The scans end one after the other without a wait, so several of
        // them end in one millisecond of the clock.
        let ends: Vec<i64> = (0..200)
            .map(|_| {
                let id = start(&db);
                end(&db, id)
            })
            .collect();
        let elapsed = now_ms() - before;

        for pair in ends.windows(2) {
            assert!(pair[0] < pair[1], "{pair:?}");
        }
        // The end times are those of the clock, and at most one millisecond
        // ahead of it for each scan.
        let last = ends[ends.len() - 1];
        assert!(last >= before, "{last} {before}");
        assert!(last <= before + elapsed + 200, "{last} {before} {elapsed}");
    }

    #[test]
    fn finish_stores_an_end_time_after_every_other_one_when_the_clock_went_back() {
        let db = Db::open_memory().unwrap();
        let first = start(&db);
        end(&db, first);
        // The first scan ended when the clock of the host was one hour
        // ahead of where it is now.
        let ahead = now_ms() + 3_600_000;
        set_finished_at(&db, first, ahead);

        let second = start(&db);
        assert_eq!(end(&db, second), ahead + 1);
        let third = start(&db);
        assert_eq!(end(&db, third), ahead + 2);

        // A scan without an end time between them has no time to compare
        // with.
        start(&db);
        let fifth = start(&db);
        assert_eq!(end(&db, fifth), ahead + 3);
    }

    #[test]
    fn finish_does_not_compare_the_end_time_with_the_one_of_its_own_row() {
        let db = Db::open_memory().unwrap();
        let id = start(&db);
        end(&db, id);
        let ahead = now_ms() + 3_600_000;
        set_finished_at(&db, id, ahead);

        // The row is closed again. No other row has an end time, so the
        // time is the one of the clock.
        let before = now_ms();
        let again = end(&db, id);
        assert!((before..=now_ms()).contains(&again), "{again}");
    }

    #[test]
    fn previous_is_open_when_the_scan_before_has_no_end_time() {
        let db = Db::open_memory().unwrap();
        let open = |id: i64| db.with_reader(|conn| previous_is_open(conn, id)).unwrap();

        // The first scan has no scan before it.
        let first = start(&db);
        assert!(!open(first));

        // The first scan has no end time when the second one starts.
        let second = start(&db);
        assert!(open(second));
        end(&db, first);
        assert!(!open(second));

        // Only the scan directly before counts.
        let third = start(&db);
        assert!(open(third));
        end(&db, second);
        assert!(!open(third));
        let fourth = start(&db);
        assert!(open(fourth));
        end(&db, third);
        assert!(!open(fourth));
        // An ID that no row has is compared with the rows before it.
        assert!(open(fourth + 10));
    }

    #[test]
    fn list_returns_every_scan_in_the_order_of_the_starts() {
        let db = Db::open_memory().unwrap();
        assert_eq!(db.with_reader(list).unwrap(), []);

        let first = start(&db);
        let second = start(&db);
        db.with_writer(|conn| finish(conn, second, &COUNTS, false, Some("cancelled")))
            .unwrap();

        let scans = db.with_reader(list).unwrap();
        let ids: Vec<i64> = scans.iter().map(|scan| scan.id).collect();
        assert_eq!(ids, [first, second]);
        assert_eq!(scans[0].finished_at, None);
        assert_eq!(scans[0].counts, ScanCounts::default());
        assert!(scans[1].finished_at.is_some());
        assert_eq!(scans[1].counts, COUNTS);
        assert!(!scans[1].changed_library);
        assert_eq!(scans[1].error.as_deref(), Some("cancelled"));
        assert_eq!(db.with_reader(latest).unwrap().as_ref(), scans.last());
    }

    #[test]
    fn last_finished_is_none_until_a_scan_has_ended() {
        let db = Db::open_memory().unwrap();
        assert_eq!(db.with_reader(last_finished).unwrap(), None);

        // A scan that runs has no end time.
        start(&db);
        assert_eq!(db.with_reader(last_finished).unwrap(), None);
    }

    #[test]
    fn last_finished_is_the_latest_end_time_in_utc() {
        let db = Db::open_memory().unwrap();
        let first = start(&db);
        let second = start(&db);
        let third = start(&db);

        set_finished_at(&db, first, (DAY + 3_723) * 1000);
        assert_eq!(
            db.with_reader(last_finished).unwrap().as_deref(),
            Some("2026-10-11T01:02:03Z")
        );

        // The scan that started last ended first. The other end time is the
        // latest. The third scan still runs.
        set_finished_at(&db, second, (DAY + 59) * 1000);
        assert_eq!(
            db.with_reader(last_finished).unwrap().as_deref(),
            Some("2026-10-11T01:02:03Z")
        );
        set_finished_at(&db, second, (DAY + 86_399) * 1000);
        assert_eq!(
            db.with_reader(last_finished).unwrap().as_deref(),
            Some("2026-10-11T23:59:59Z")
        );
        assert_eq!(db.with_reader(latest).unwrap().unwrap().id, third);
    }

    #[test]
    fn last_finished_cuts_the_milliseconds_off() {
        let db = Db::open_memory().unwrap();
        let id = start(&db);
        // Every millisecond of one second gives the text of that second.
        for millisecond in [0, 1, 499, 500, 999] {
            set_finished_at(&db, id, (DAY + 3_723) * 1000 + millisecond);
            assert_eq!(
                db.with_reader(last_finished).unwrap().as_deref(),
                Some("2026-10-11T01:02:03Z"),
                "{millisecond}"
            );
        }
        set_finished_at(&db, id, (DAY + 3_724) * 1000);
        assert_eq!(
            db.with_reader(last_finished).unwrap().as_deref(),
            Some("2026-10-11T01:02:04Z")
        );
    }

    #[test]
    fn last_finished_counts_a_scan_that_ended_with_an_error_and_one_without_a_change() {
        let db = Db::open_memory().unwrap();
        let id = start(&db);
        db.with_writer(|conn| finish(conn, id, &COUNTS, false, Some("cancelled")))
            .unwrap();
        set_finished_at(&db, id, 0);
        assert_eq!(
            db.with_reader(last_finished).unwrap().as_deref(),
            Some("1970-01-01T00:00:00Z")
        );
    }

    #[test]
    fn last_finished_of_a_scan_that_ends_now_is_the_present_second() {
        let db = Db::open_memory().unwrap();
        let id = start(&db);
        let finished_at = end(&db, id);
        let expected: String = db
            .with_reader(|conn| {
                Ok(conn.query_row(
                    "SELECT strftime('%Y-%m-%dT%H:%M:%SZ', ?1, 'unixepoch')",
                    [finished_at / 1000],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        // The text names a time of this century, so the stored value is
        // not read as seconds.
        assert!(expected.starts_with("20"), "{expected}");
        assert_eq!(db.with_reader(last_finished).unwrap(), Some(expected));
    }

    #[test]
    fn latest_returns_the_scan_that_started_last() {
        let db = Db::open_memory().unwrap();
        let first = start(&db);
        let second = start(&db);
        db.with_writer(|conn| finish(conn, first, &COUNTS, true, None))
            .unwrap();

        let scan = db.with_reader(latest).unwrap().unwrap();
        assert_ne!(first, second);
        assert_eq!(scan.id, second);
        assert_eq!(scan.finished_at, None);
    }
}
