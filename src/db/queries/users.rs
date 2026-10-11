//! Queries on the `users` table. The table holds one account, the owner.
//!
//! The password column is plain text (`docs/design.md` section 1.3). Every
//! statement binds the password as a parameter, so it is not part of the SQL
//! text that an error can carry.

use std::fmt;

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::db::Result;

/// The value of `users.role` for the owner account.
pub const ROLE_OWNER: &str = "owner";

/// A user account without its password.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    /// The integer ID that the per-user tables reference.
    pub id: i64,
    /// The name as it was stored. Lookups by name ignore ASCII case.
    pub name: String,
    /// The role. [`ROLE_OWNER`] is the only role.
    pub role: String,
}

/// A row of the `users` table: the account, its stored password and its
/// creation time.
///
/// The `Debug` output leaves the password out.
#[derive(Clone, PartialEq, Eq)]
pub struct UserRow {
    /// The account.
    pub user: User,
    /// The stored password, in plain text.
    pub password: String,
    /// The creation time in Unix seconds.
    pub created_at: i64,
}

impl fmt::Debug for UserRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UserRow")
            .field("user", &self.user)
            .field("created_at", &self.created_at)
            .finish_non_exhaustive()
    }
}

/// The columns of a [`UserRow`], in the order `row_from_row` reads them.
const ROW_COLUMNS: &str = "id, name, role, password, created_at";

/// Inserts the owner account and returns it. Returns `None` and changes
/// nothing when an owner account exists.
///
/// The check and the insert are one statement, so two callers cannot both
/// insert an owner.
pub fn insert_owner(conn: &Connection, name: &str, password: &str) -> Result<Option<User>> {
    let user = conn
        .query_row(
            "INSERT INTO users (name, role, password, created_at, updated_at)
             SELECT ?1, ?2, ?3, unixepoch(), unixepoch()
             WHERE NOT EXISTS (SELECT 1 FROM users WHERE role = ?2)
             RETURNING id, name, role",
            params![name, ROLE_OWNER, password],
            user_from_row,
        )
        .optional()?;
    Ok(user)
}

/// Returns the row of the user with this name, or `None`. The comparison
/// ignores ASCII case, as the collation of the `name` column defines.
pub fn get_by_name(conn: &Connection, name: &str) -> Result<Option<UserRow>> {
    let row = conn
        .query_row(
            &format!("SELECT {ROW_COLUMNS} FROM users WHERE name = ?1"),
            [name],
            row_from_row,
        )
        .optional()?;
    Ok(row)
}

/// Returns the row of the owner account, or `None` when the table has none.
pub fn get_owner(conn: &Connection) -> Result<Option<UserRow>> {
    let row = conn
        .query_row(
            &format!("SELECT {ROW_COLUMNS} FROM users WHERE role = ?1 ORDER BY id LIMIT 1"),
            [ROLE_OWNER],
            row_from_row,
        )
        .optional()?;
    Ok(row)
}

/// Replaces the stored password of the user with this ID and sets the update
/// time. Returns `false` and changes nothing when no user has the ID.
pub fn set_password(conn: &Connection, user_id: i64, password: &str) -> Result<bool> {
    let changed = conn.execute(
        "UPDATE users SET password = ?2, updated_at = unixepoch() WHERE id = ?1",
        params![user_id, password],
    )?;
    Ok(changed == 1)
}

fn user_from_row(row: &Row<'_>) -> rusqlite::Result<User> {
    Ok(User {
        id: row.get(0)?,
        name: row.get(1)?,
        role: row.get(2)?,
    })
}

fn row_from_row(row: &Row<'_>) -> rusqlite::Result<UserRow> {
    Ok(UserRow {
        user: user_from_row(row)?,
        password: row.get(3)?,
        created_at: row.get(4)?,
    })
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use tempfile::TempDir;

    use super::*;
    use crate::db::Db;

    /// Returns an in-memory database and a temp-file database. The directory
    /// guard keeps the file for the life of the test.
    fn both_dbs() -> (TempDir, [Db; 2]) {
        let dir = TempDir::new().unwrap();
        let file_db = Db::open(dir.path().join("library.db")).unwrap();
        (dir, [Db::open_memory().unwrap(), file_db])
    }

    fn unix_now() -> i64 {
        let elapsed = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        i64::try_from(elapsed.as_secs()).unwrap()
    }

    fn owner(db: &Db) -> UserRow {
        db.with_reader(get_owner).unwrap().expect("an owner")
    }

    fn count_users(db: &Db) -> i64 {
        db.with_reader(|conn| {
            Ok(conn.query_row("SELECT count(*) FROM users", [], |row| row.get(0))?)
        })
        .unwrap()
    }

    /// Returns `created_at` and `updated_at` of the user with this ID.
    fn times(db: &Db, user_id: i64) -> (i64, i64) {
        db.with_reader(|conn| {
            Ok(conn.query_row(
                "SELECT created_at, updated_at FROM users WHERE id = ?1",
                [user_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?)
        })
        .unwrap()
    }

    #[test]
    fn insert_owner_stores_the_account_with_the_owner_role() {
        let (_dir, dbs) = both_dbs();
        for db in dbs {
            let before = unix_now();
            let user = db
                .with_writer(|conn| insert_owner(conn, "adnan", "sesame"))
                .unwrap()
                .expect("inserted");
            let after = unix_now();
            assert_eq!(
                user,
                User {
                    id: 1,
                    name: "adnan".to_string(),
                    role: "owner".to_string(),
                }
            );

            let row = owner(&db);
            assert_eq!(row.user, user);
            assert_eq!(row.password, "sesame");
            assert!((before..=after).contains(&row.created_at), "{row:?}");
            assert_eq!(times(&db, user.id), (row.created_at, row.created_at));
        }
    }

    #[test]
    fn insert_owner_does_not_replace_an_owner() {
        let (_dir, dbs) = both_dbs();
        for db in dbs {
            let first = db
                .with_writer(|conn| insert_owner(conn, "adnan", "sesame"))
                .unwrap()
                .expect("inserted");
            for name in ["adnan", "other"] {
                let second = db
                    .with_writer(|conn| insert_owner(conn, name, "changed"))
                    .unwrap();
                assert_eq!(second, None, "{name}");
            }
            assert_eq!(count_users(&db), 1);
            let row = owner(&db);
            assert_eq!(row.user, first);
            assert_eq!(row.password, "sesame");
        }
    }

    #[test]
    fn get_owner_returns_none_without_an_account() {
        let (_dir, dbs) = both_dbs();
        for db in dbs {
            assert_eq!(db.with_reader(get_owner).unwrap(), None);
        }
    }

    #[test]
    fn get_by_name_ignores_ascii_case_and_returns_the_stored_name() {
        let (_dir, dbs) = both_dbs();
        for db in dbs {
            db.with_writer(|conn| insert_owner(conn, "Adnan", "sesame"))
                .unwrap();
            for name in ["Adnan", "adnan", "ADNAN"] {
                let row = db
                    .with_reader(|conn| get_by_name(conn, name))
                    .unwrap()
                    .unwrap_or_else(|| panic!("{name}"));
                assert_eq!(row, owner(&db), "{name}");
                assert_eq!(row.user.name, "Adnan");
                assert_eq!(row.password, "sesame");
            }
        }
    }

    #[test]
    fn get_by_name_returns_none_for_an_unknown_name() {
        let (_dir, dbs) = both_dbs();
        for db in dbs {
            db.with_writer(|conn| insert_owner(conn, "adnan", "sesame"))
                .unwrap();
            for name in ["", "adna", "adnan ", "owner", "%"] {
                let row = db.with_reader(|conn| get_by_name(conn, name)).unwrap();
                assert_eq!(row, None, "{name:?}");
            }
        }
    }

    #[test]
    fn set_password_replaces_the_password_and_sets_the_update_time() {
        let (_dir, dbs) = both_dbs();
        for db in dbs {
            let user = db
                .with_writer(|conn| insert_owner(conn, "adnan", "sesame"))
                .unwrap()
                .expect("inserted");
            // Both times move to the past, so the update is visible without
            // waiting for the clock.
            db.with_writer(|conn| {
                conn.execute("UPDATE users SET created_at = 10, updated_at = 10", [])?;
                Ok(())
            })
            .unwrap();

            let before = unix_now();
            let changed = db
                .with_writer(|conn| set_password(conn, user.id, "changed"))
                .unwrap();
            assert!(changed);

            let row = owner(&db);
            assert_eq!(row.user, user);
            assert_eq!(row.password, "changed");
            assert_eq!(row.created_at, 10);
            let (created_at, updated_at) = times(&db, user.id);
            assert_eq!(created_at, 10);
            assert!(updated_at >= before, "{updated_at}");
        }
    }

    #[test]
    fn set_password_returns_false_for_an_unknown_user() {
        let (_dir, dbs) = both_dbs();
        for db in dbs {
            let user = db
                .with_writer(|conn| insert_owner(conn, "adnan", "sesame"))
                .unwrap()
                .expect("inserted");
            let changed = db
                .with_writer(|conn| set_password(conn, user.id + 1, "changed"))
                .unwrap();
            assert!(!changed);
            assert_eq!(owner(&db).password, "sesame");
        }
    }

    #[test]
    fn a_password_is_stored_as_given() {
        let db = Db::open_memory().unwrap();
        let password = "  it's \"quoted\"; -- ☃\n";
        let user = db
            .with_writer(|conn| insert_owner(conn, "adnan", password))
            .unwrap()
            .expect("inserted");
        assert_eq!(owner(&db).password, password);

        let other = "'); DROP TABLE users; --";
        db.with_writer(|conn| set_password(conn, user.id, other))
            .unwrap();
        assert_eq!(owner(&db).password, other);
    }

    #[test]
    fn debug_output_of_a_row_leaves_the_password_out() {
        let db = Db::open_memory().unwrap();
        db.with_writer(|conn| insert_owner(conn, "adnan", "sesame-secret"))
            .unwrap();
        let text = format!("{:?}", owner(&db));
        assert!(text.contains("adnan"), "{text}");
        assert!(!text.contains("sesame-secret"), "{text}");
        assert!(!text.contains("password"), "{text}");
    }
}
