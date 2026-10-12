//! Conversions between the unsigned 64-bit integers of the models and the
//! signed 64-bit integers that SQLite stores.

use rusqlite::{Error, Row};

/// Returns `value` as the integer that SQLite stores. A value above
/// `i64::MAX` is an error.
pub(super) fn to_sql(value: u64) -> rusqlite::Result<i64> {
    i64::try_from(value).map_err(|error| Error::ToSqlConversionFailure(Box::new(error)))
}

/// Reads the column `index` of `row` as an unsigned integer. A negative
/// value is an error.
pub(super) fn get_u64(row: &Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|_| Error::IntegralValueOutOfRange(index, value))
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;

    #[test]
    fn to_sql_keeps_the_value_and_rejects_one_above_the_signed_range() {
        assert_eq!(to_sql(0).unwrap(), 0);
        assert_eq!(to_sql(i64::MAX as u64).unwrap(), i64::MAX);
        let error = to_sql(i64::MAX as u64 + 1).unwrap_err();
        assert!(matches!(error, Error::ToSqlConversionFailure(_)), "{error}");
    }

    #[test]
    fn get_u64_reads_the_value_and_rejects_a_negative_one() {
        let conn = Connection::open_in_memory().unwrap();
        let read = |sql: &str| conn.query_row(sql, [], |row| get_u64(row, 0));
        assert_eq!(read("SELECT 0").unwrap(), 0);
        assert_eq!(read("SELECT 9223372036854775807").unwrap(), i64::MAX as u64);
        let error = read("SELECT -1").unwrap_err();
        assert!(
            matches!(error, Error::IntegralValueOutOfRange(0, -1)),
            "{error}"
        );
    }
}
