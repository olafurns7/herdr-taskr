use rusqlite::{Connection, TransactionBehavior};
use std::path::Path;

pub const SCHEMA: &str = include_str!("../../../schema.sql");
pub const SEARCH_TRIGGER: &str = include_str!("search-trigger.sql");
pub const SEARCH_REBUILD: &str = include_str!("search-rebuild.sql");
/// Rust-only tables (v2: `taskr after`); schema.sql stays the frozen Go schema.
pub const SUBSCRIPTIONS: &str = "create table if not exists subscriptions (
  id             integer primary key,
  waiter_task_id integer not null references tasks(id),
  target         text not null,
  kinds          text not null,
  keep           integer not null default 0,
  created_at     text not null,
  fired_at       text
);
create index if not exists subscriptions_open on subscriptions(target) where fired_at is null;
";
pub const SCHEMA_VERSION: i64 = 2;
#[cfg(test)]
const SCHEMA_HASH: &str = "1a248ec37a3a377fac87702a248b3ac120c23b50d686e0da9c9fb5553e4b54eb";

pub fn open(path: &Path) -> rusqlite::Result<Connection> {
    let mut db = connect(path, rusqlite::OpenFlags::default())?;
    let version: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version < SCHEMA_VERSION || version == SCHEMA_VERSION && !search_intact(&db)? {
        migrate(&mut db)?;
    }
    Ok(db)
}

pub(crate) fn connect(path: &Path, flags: rusqlite::OpenFlags) -> rusqlite::Result<Connection> {
    let db = Connection::open_with_flags(path, flags)?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    db.pragma_update(None, "journal_mode", "WAL")?;
    db.pragma_update(None, "foreign_keys", true)?;
    Ok(db)
}

fn search_intact(db: &Connection) -> rusqlite::Result<bool> {
    if !db.table_exists(None, "search_fts")? {
        return Ok(false);
    }
    let (trigger, secure_delete): (String, bool) = db.query_row("select coalesce((select sql from sqlite_master where type='trigger' and name='search_events_insert'), ''), coalesce((select v from search_fts_config where k='secure-delete') = 1, 0)", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(trigger == SEARCH_TRIGGER && secure_delete)
}

// Every migration change needs a SCHEMA_VERSION bump because current ledgers skip it.
pub fn migrate(db: &mut Connection) -> rusqlite::Result<()> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(SCHEMA)?;
    tx.execute_batch(SUBSCRIPTIONS)?;
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
    if search_intact(&tx)? {
        tx.execute_batch("insert into search_fts(search_fts, rank) values('secure-delete', 1)")?;
    } else {
        tx.execute_batch(SEARCH_REBUILD)?;
    }
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn migration_effect_requires_schema_version_bump() {
        use sha2::{Digest, Sha256};
        let mut db = Connection::open_in_memory().unwrap();
        migrate(&mut db).unwrap();
        let schema = db
            .prepare("select type,name,tbl_name,sql from sqlite_master order by type,name")
            .unwrap()
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        let mut columns = Vec::new();
        for (_, name, _, _) in schema.iter().filter(|r| r.0 == "table") {
            let info = db.prepare("select cid,name,type,\"notnull\",dflt_value,pk from pragma_table_info(?) order by cid")
                .unwrap().query_map([name], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, r.get::<_, Option<String>>(4)?, r.get::<_, i64>(5)?)))
                .unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
            columns.push((name, info));
        }
        let effect = serde_json::to_vec(&(&schema, columns, SEARCH_REBUILD)).unwrap();
        assert_eq!(
            format!("{:x}", Sha256::digest(effect)),
            SCHEMA_HASH,
            "bump SCHEMA_VERSION and pin the new migration effect"
        );
    }
    #[test]
    fn probe_failed_migration_leaves_user_version_unset() {
        let root = std::env::temp_dir().join(format!("taskr-schema-failed-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("ledger.db");
        let db = Connection::open(&path).unwrap();
        // An index named like the first table makes its CREATE TABLE fail.
        db.execute_batch("create table x(a); create index tasks on x(a)")
            .unwrap();
        drop(db);
        assert!(open(&path).is_err());
        let db = Connection::open(&path).unwrap();
        assert_eq!(
            db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn fresh_unversioned_current_and_newer_opens() {
        let root =
            std::env::temp_dir().join(format!("taskr-schema-version-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("ledger.db");
        let db = open(&path).unwrap();
        let version = |db: &Connection| {
            db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap()
        };
        assert_eq!(version(&db), SCHEMA_VERSION);
        db.execute_batch("pragma user_version=0; drop trigger search_events_insert")
            .unwrap();
        drop(db);
        let db = open(&path).unwrap();
        assert_eq!(version(&db), SCHEMA_VERSION);
        assert!(
            db.query_row(
                "select exists(select 1 from sqlite_master where name='search_events_insert')",
                [],
                |r| r.get::<_, bool>(0)
            )
            .unwrap()
        );
        for expected in [SCHEMA_VERSION, SCHEMA_VERSION + 1] {
            db.pragma_update(None, "user_version", expected).unwrap();
            db.execute_batch("begin immediate").unwrap();
            let reader = open(&path).unwrap();
            assert_eq!(version(&reader), expected);
            assert_eq!(
                reader
                    .query_row("select count(*) from tasks", [], |r| r.get::<_, i64>(0))
                    .unwrap(),
                0
            );
            db.execute_batch("rollback").unwrap();
            drop(reader);
        }
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn v1_ledger_migrates_to_v2_once_and_idempotently() {
        let root = std::env::temp_dir().join(format!("taskr-schema-v1-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("ledger.db");
        // A v1 ledger as v0.17.x left it: the schema, the search index, user_version 1.
        let db = Connection::open(&path).unwrap();
        db.execute_batch(SCHEMA).unwrap();
        db.execute_batch(SEARCH_REBUILD).unwrap();
        db.pragma_update(None, "user_version", 1).unwrap();
        db.execute("insert into tasks(name,role,created_at,updated_at) values('kept','orchestrator','t','t')", []).unwrap();
        drop(db);
        let shape = |db: &Connection| {
            db.query_row(
                "select (select user_version from pragma_user_version), (select count(*) from tasks), (select count(*) from subscriptions), (select sql from sqlite_master where name='subscriptions_open')",
                [],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?)),
            )
            .unwrap()
        };
        let want = (
            2,
            1,
            0,
            "CREATE INDEX subscriptions_open on subscriptions(target) where fired_at is null"
                .to_string(),
        );
        // A hub child of a daemon not yet restarted (open_migrated) migrates it once.
        let db = crate::db::open_migrated(&path).unwrap();
        assert_eq!(shape(&db), want);
        drop(db);
        let mut db = open(&path).unwrap();
        migrate(&mut db).unwrap();
        assert_eq!(shape(&db), want);
        assert_eq!(
            db.query_row("pragma integrity_check", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }
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
