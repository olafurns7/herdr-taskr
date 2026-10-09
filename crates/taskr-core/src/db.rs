//! Shared local-ledger opener. Never used by client-mode routing.
pub use rusqlite::{self, Connection, OptionalExtension, Row, params, params_from_iter};
use std::path::{Path, PathBuf};
pub fn path() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("TASKR_DB")
        && !p.is_empty()
    {
        return Ok(p.into());
    }
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        return Err("HOME is not set and TASKR_DB is empty".into());
    }
    Ok(PathBuf::from(home).join(".local/state/taskr/taskr.db"))
}
pub fn open(path: &Path) -> Result<Connection, String> {
    if crate::store::rpc_context().is_some() {
        return open_migrated(path);
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| {
            if let Some(file) = parent
                .ancestors()
                .find(|p| std::fs::metadata(p).is_ok_and(|m| !m.is_dir()))
            {
                return format!("mkdir {}: not a directory", file.display());
            }
            let message = e.to_string();
            let message = message
                .split(" (os error")
                .next()
                .unwrap_or("error")
                .to_lowercase();
            format!("mkdir {}: {message}", parent.display())
        })?;
    }
    crate::schema::open(path).map_err(|e| error_text(&e))
}
/// The daemon already migrated this ledger; RPC reads must not take a write lock.
/// A new binary under a daemon not yet restarted migrates an older ledger once.
pub fn open_migrated(path: &Path) -> Result<Connection, String> {
    let db = crate::schema::connect(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(|e| error_text(&e))?;
    let version: i64 = db
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(|e| error_text(&e))?;
    if version < crate::schema::SCHEMA_VERSION {
        drop(db);
        return crate::schema::open(path).map_err(|e| error_text(&e));
    }
    Ok(db)
}
pub fn busy_reply(exit: i64, error: &str) -> bool {
    exit == 4 && error.starts_with("database is locked")
}
pub fn error_text(e: &rusqlite::Error) -> String {
    match e {
        rusqlite::Error::SqlInputError { error, msg, .. } => {
            error_text(&rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(error.extended_code),
                Some(msg.clone()),
            ))
        }
        rusqlite::Error::QueryReturnedNoRows => "sql: no rows in result set".into(),
        rusqlite::Error::SqliteFailure(code, Some(message)) => {
            let category = match code.extended_code & 255 {
                1 => "SQL logic error",
                2 => "unknown error",
                3 => "access permission denied",
                4 => "query aborted",
                5 => "database is locked",
                6 => "database table is locked",
                7 => "out of memory",
                8 => "attempt to write a readonly database",
                9 => "interrupted",
                10 => "disk I/O error",
                11 => "database disk image is malformed",
                12 => "unknown operation",
                13 => "database or disk is full",
                14 => "unable to open database file",
                15 => "locking protocol",
                16 => "unknown error",
                17 => "database schema has changed",
                18 => "string or blob too big",
                19 => "constraint failed",
                20 => "datatype mismatch",
                21 => "bad parameter or other API misuse",
                22 => "unknown error",
                23 => "authorization denied",
                24 => "unknown error",
                25 => "column index out of range",
                26 => "file is not a database",
                27 => "notification message",
                28 => "warning message",
                _ => "unknown error",
            };
            let body = if *message == category || code.extended_code == 14 {
                category.to_string()
            } else {
                format!("{category}: {message}")
            };
            format!(
                "{body} ({}){}",
                code.extended_code,
                if code.extended_code == 5 {
                    " (SQLITE_BUSY)"
                } else {
                    ""
                }
            )
        }
        _ => e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn busy_reply_requires_database_exit_and_busy_prefix() {
        for code in [5, 261, 517, 773] {
            assert!(busy_reply(4, &format!("database is locked ({code})")));
        }
        assert!(!busy_reply(5, "database is locked (5) (SQLITE_BUSY)"));
        for error in [
            "database table is locked (6)",
            "disk I/O error (10)",
            "unrelated SQLITE_BUSY text",
        ] {
            assert!(!busy_reply(4, error));
        }
    }
    #[test]
    fn migrated_reader_does_not_take_the_writer_lock_or_create_a_ledger() {
        let root = std::env::temp_dir().join(format!("taskr-migrated-db-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("ledger.db");
        let writer = open(&path).unwrap();
        writer.execute_batch("begin immediate").unwrap();
        let reader = open_migrated(&path).unwrap();
        let count: i64 = reader
            .query_row("select count(*) from tasks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
        writer.execute_batch("rollback").unwrap();
        drop(reader);
        drop(writer);
        let absent = root.join("absent.db");
        assert!(open_migrated(&absent).is_err());
        assert!(!absent.exists());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn open_pragmas_and_go_error_text() {
        let root = std::env::temp_dir().join(format!("taskr-read-db-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let db = open(&root.join("nested/ledger.db")).unwrap();
        assert_eq!(
            db.query_row("pragma journal_mode", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "wal"
        );
        assert_eq!(
            db.query_row("pragma busy_timeout", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            5000
        );
        assert_eq!(
            db.query_row("pragma foreign_keys", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        let err = db.execute_batch("select missing_column").unwrap_err();
        assert_eq!(
            error_text(&err),
            "SQL logic error: no such column: missing_column (1)"
        );
        drop(db);
        std::fs::write(root.join("file"), "not a database").unwrap();
        assert_eq!(
            open(&root.join("file/db")).unwrap_err(),
            format!("mkdir {}: not a directory", root.join("file").display())
        );
        assert_eq!(
            open(&root).unwrap_err(),
            "unable to open database file (14)"
        );
        assert_eq!(
            open(&root.join("file")).unwrap_err(),
            "file is not a database (26)"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
