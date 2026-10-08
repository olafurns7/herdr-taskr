use rusqlite::{Connection, TransactionBehavior};
use std::path::Path;

pub const SCHEMA: &str = include_str!("../../../schema.sql");
pub const SEARCH_TRIGGER: &str = include_str!("search-trigger.sql");
pub const SEARCH_REBUILD: &str = include_str!("search-rebuild.sql");

pub fn open(path: &Path) -> rusqlite::Result<Connection> {
    let mut db = connect(path, rusqlite::OpenFlags::default())?;
    migrate(&mut db)?;
    Ok(db)
}

pub(crate) fn connect(path: &Path, flags: rusqlite::OpenFlags) -> rusqlite::Result<Connection> {
    let db = Connection::open_with_flags(path, flags)?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    db.pragma_update(None, "journal_mode", "WAL")?;
    db.pragma_update(None, "foreign_keys", true)?;
    Ok(db)
}

pub fn migrate(db: &mut Connection) -> rusqlite::Result<()> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(SCHEMA)?;
    for (table, column, kind) in [
        ("tasks", "waiting_until", "text"),
        ("launches", "workspace_id", "text"),
        ("launches", "tab_id", "text"),
        ("tasks", "machine", "text"),
        ("launches", "machine", "text"),
        ("launches", "transcript_path", "text"),
        ("requests", "upload", "text"),
        ("tasks", "lead_status", "text"),
        ("tasks", "lead_present", "integer"),
        ("tasks", "lead_observed_at", "text"),
    ] {
        let n: i64 = tx.query_row(
            "select count(*) from pragma_table_info(?) where name = ?",
            [table, column],
            |row| row.get(0),
        )?;
        if n == 0 {
            tx.execute_batch(&format!("alter table {table} add column {column} {kind}"))?;
        }
    }
    let (exists, trigger): (bool, String) = tx.query_row("select exists(select 1 from sqlite_master where type = 'table' and name = 'search_fts'), coalesce((select sql from sqlite_master where type = 'trigger' and name = 'search_events_insert'), '')", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
    if exists && trigger == SEARCH_TRIGGER {
        tx.execute_batch("insert into search_fts(search_fts, rank) values('secure-delete', 1)")?;
    } else {
        tx.execute_batch(SEARCH_REBUILD)?;
    }
    tx.commit()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_and_current_are_idempotent() {
        for schema in [
            "",
            include_str!("../../../testdata/taskr-v0.9.1-schema.sql"),
        ] {
            let mut db = Connection::open_in_memory().unwrap();
            db.execute_batch(schema).unwrap();
            migrate(&mut db).unwrap();
            migrate(&mut db).unwrap();
            let n: i64 = db
                .query_row(
                    "select count(*) from pragma_table_info('tasks') where name = 'lead_present'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1);
            assert_eq!(
                db.query_row("pragma integrity_check", [], |r| r.get::<_, String>(0))
                    .unwrap(),
                "ok"
            );
        }
    }
}
