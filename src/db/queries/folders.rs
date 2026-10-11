//! Queries on the `folders` table. A row is one configured music folder.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, Row};

use crate::db::{DbError, Result};

/// A music folder row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    /// The integer ID that Subsonic clients see.
    pub id: i64,
    /// The absolute path of the folder in its stored form: the components
    /// joined with the separator of the OS, without a trailing separator.
    pub path: PathBuf,
    /// The display name: the last component of the path, or the whole path
    /// when it has none.
    pub name: String,
}

/// Inserts a row for each path that has none and returns the rows of `paths`
/// in the order of first occurrence. A path that already has a row keeps its
/// ID.
///
/// A path is stored as its components joined with the separator of the OS.
/// Spellings of one directory that differ by a trailing separator, a `.`
/// component or a repeated separator share a row.
///
/// Rows of other paths stay in the table. The scanner deletes them after it
/// has matched moved tracks.
///
/// The statements run on `conn` without a transaction of their own. A path
/// that is not valid Unicode is an error.
pub fn sync_folders(conn: &Connection, paths: &[PathBuf]) -> Result<Vec<Folder>> {
    let mut upsert = conn.prepare(
        "INSERT INTO folders (path, name) VALUES (?1, ?2)
         ON CONFLICT (path) DO UPDATE SET name = excluded.name
         RETURNING id, path, name",
    )?;
    let mut folders: Vec<Folder> = Vec::with_capacity(paths.len());
    for path in paths {
        let stored = stored_form(path);
        let text = stored
            .to_str()
            .ok_or_else(|| DbError::NonUnicodePath(path.clone()))?;
        let folder = upsert.query_row([text, display_name(&stored, text)], folder_from_row)?;
        if !folders.iter().any(|known| known.id == folder.id) {
            folders.push(folder);
        }
    }
    Ok(folders)
}

/// Rebuilds `path` from its components. The result has no trailing
/// separator, no repeated separator and no `.` component, except a `.` that
/// starts a relative path. `..` components stay, because removing one changes
/// the directory when the component before it is a symlink.
fn stored_form(path: &Path) -> PathBuf {
    path.components().collect()
}

/// Returns every folder row in ID order.
pub fn list(conn: &Connection) -> Result<Vec<Folder>> {
    let mut select = conn.prepare("SELECT id, path, name FROM folders ORDER BY id")?;
    let folders = select
        .query_map([], folder_from_row)?
        .collect::<rusqlite::Result<Vec<Folder>>>()?;
    Ok(folders)
}

/// Returns the last component of `path`, or `text`, the whole path, when the
/// path has no last component (a root such as `/` or `C:\`).
fn display_name<'a>(path: &'a Path, text: &'a str) -> &'a str {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(text)
}

fn folder_from_row(row: &Row<'_>) -> rusqlite::Result<Folder> {
    let path: String = row.get(1)?;
    Ok(Folder {
        id: row.get(0)?,
        path: PathBuf::from(path),
        name: row.get(2)?,
    })
}

#[cfg(test)]
mod tests {
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

    fn paths(texts: &[&str]) -> Vec<PathBuf> {
        texts.iter().map(PathBuf::from).collect()
    }

    fn sync(db: &Db, texts: &[&str]) -> Vec<Folder> {
        let paths = paths(texts);
        db.with_writer(|conn| sync_folders(conn, &paths)).unwrap()
    }

    /// Returns the path text of a row. `PathBuf` equality ignores a trailing
    /// separator, so tests of the stored form compare text.
    fn stored_text(folder: &Folder) -> String {
        folder.path.to_str().unwrap().to_string()
    }

    /// Returns `text`, which uses `/`, with the separator of this OS.
    fn os_text(text: &str) -> String {
        text.replace('/', std::path::MAIN_SEPARATOR_STR)
    }

    #[test]
    fn sync_folders_inserts_rows_and_returns_them_in_input_order() {
        let (_dir, dbs) = both_dbs();
        for db in dbs {
            let folders = sync(&db, &["/music/rock", "/music/jazz"]);
            assert_eq!(
                folders,
                [
                    Folder {
                        id: 1,
                        path: PathBuf::from("/music/rock"),
                        name: "rock".to_string(),
                    },
                    Folder {
                        id: 2,
                        path: PathBuf::from("/music/jazz"),
                        name: "jazz".to_string(),
                    },
                ]
            );
            assert_eq!(db.with_reader(list).unwrap(), folders);
        }
    }

    #[test]
    fn sync_folders_keeps_the_id_of_a_known_path() {
        let (_dir, dbs) = both_dbs();
        for db in dbs {
            let first = sync(&db, &["/music/rock", "/music/jazz"]);
            let second = sync(&db, &["/music/jazz", "/music/pop", "/music/rock"]);
            let ids: Vec<i64> = second.iter().map(|folder| folder.id).collect();
            assert_eq!(ids, [first[1].id, 3, first[0].id]);
            assert_eq!(db.with_reader(list).unwrap().len(), 3);
        }
    }

    #[test]
    fn sync_folders_does_not_delete_a_folder_that_is_no_longer_listed() {
        let (_dir, dbs) = both_dbs();
        for db in dbs {
            let first = sync(&db, &["/music/rock", "/music/jazz"]);
            let second = sync(&db, &["/music/jazz"]);
            assert_eq!(second, [first[1].clone()]);
            assert_eq!(db.with_reader(list).unwrap(), first);
        }
    }

    #[test]
    fn sync_folders_returns_one_row_for_a_repeated_path() {
        let (_dir, dbs) = both_dbs();
        for db in dbs {
            let folders = sync(&db, &["/music/rock", "/music/jazz", "/music/rock"]);
            let ids: Vec<i64> = folders.iter().map(|folder| folder.id).collect();
            assert_eq!(ids, [1, 2]);
            assert_eq!(db.with_reader(list).unwrap(), folders);
        }
    }

    #[test]
    fn sync_folders_with_no_paths_returns_no_rows() {
        let (_dir, dbs) = both_dbs();
        for db in dbs {
            assert_eq!(sync(&db, &[]), []);
            assert_eq!(db.with_reader(list).unwrap(), []);
        }
    }

    #[test]
    fn sync_folders_stores_one_row_for_spellings_of_one_directory() {
        let (_dir, dbs) = both_dbs();
        for db in dbs {
            let spellings = [
                "/music/jazz/",
                "/music/jazz",
                "/music/./jazz",
                "/music//jazz",
                "/music/jazz/.",
            ];
            let folders = sync(&db, &spellings);
            assert_eq!(folders.len(), 1);
            assert_eq!(folders[0].id, 1);
            assert_eq!(stored_text(&folders[0]), os_text("/music/jazz"));
            assert_eq!(folders[0].name, "jazz");
            assert_eq!(db.with_reader(list).unwrap(), folders);
        }
    }

    #[test]
    fn sync_folders_keeps_the_row_when_a_trailing_separator_is_added() {
        let (_dir, dbs) = both_dbs();
        for db in dbs {
            let first = sync(&db, &["/music/jazz"]);
            let second = sync(&db, &["/music/jazz/"]);
            assert_eq!(second, first);
            assert_eq!(db.with_reader(list).unwrap(), first);
        }
    }

    #[test]
    fn sync_folders_keeps_parent_components() {
        let db = Db::open_memory().unwrap();
        let folders = sync(&db, &["/music/rock/../jazz", "/music/jazz"]);
        let texts: Vec<String> = folders.iter().map(stored_text).collect();
        assert_eq!(
            texts,
            [os_text("/music/rock/../jazz"), os_text("/music/jazz")]
        );
    }

    #[test]
    fn folder_name_is_the_last_component_or_the_whole_path() {
        let db = Db::open_memory().unwrap();
        let folders = sync(&db, &["/music/Rock Albums", "/music/jazz/", "/"]);
        let names: Vec<&str> = folders.iter().map(|folder| folder.name.as_str()).collect();
        assert_eq!(names, ["Rock Albums", "jazz", os_text("/").as_str()]);
    }

    #[test]
    fn list_returns_rows_in_id_order() {
        let (_dir, dbs) = both_dbs();
        for db in dbs {
            sync(&db, &["/music/b", "/music/a", "/music/c"]);
            let listed = db.with_reader(list).unwrap();
            let seen: Vec<(i64, String)> = listed
                .iter()
                .map(|folder| (folder.id, stored_text(folder)))
                .collect();
            assert_eq!(
                seen,
                [
                    (1, os_text("/music/b")),
                    (2, os_text("/music/a")),
                    (3, os_text("/music/c")),
                ]
            );
        }
    }

    // A path that is not valid Unicode is built from raw bytes, which only
    // Unix paths allow.
    #[cfg(unix)]
    #[test]
    fn sync_folders_rejects_a_path_that_is_not_unicode() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let db = Db::open_memory().unwrap();
        let path = PathBuf::from(OsStr::from_bytes(b"/music/\xff"));
        let error = db
            .with_writer(|conn| sync_folders(conn, std::slice::from_ref(&path)))
            .unwrap_err();
        assert!(
            matches!(&error, DbError::NonUnicodePath(rejected) if *rejected == path),
            "{error}"
        );
        assert_eq!(db.with_reader(list).unwrap(), []);
    }
}
