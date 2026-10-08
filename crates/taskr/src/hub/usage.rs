use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Mutex};
use taskr_core::{compact_json, db::Connection, store};
use time::{Duration, OffsetDateTime};

const CLASSES: [&str; 6] = [
    "page",
    "state_loopback",
    "state_tailnet",
    "push_accepted",
    "refused",
    "other",
];
type Counts = BTreeMap<String, BTreeMap<String, i64>>;

#[derive(Default)]
pub(crate) struct Usage {
    pending: Mutex<Counts>,
}

fn hour(at: OffsetDateTime) -> String {
    let at = at.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour()
    )
}

impl Usage {
    pub(super) fn record(&self, status: u16, at: OffsetDateTime) {
        let mut pending = self.pending.lock().expect("usage mutex");
        let class = if matches!(status, 403 | 421) {
            "refused"
        } else {
            "other"
        };
        *pending
            .entry(hour(at))
            .or_default()
            .entry(class.into())
            .or_default() += 1;
    }

    pub(crate) fn flush(&self, db: &mut Connection, at: OffsetDateTime) -> store::Result<()> {
        let mut pending = self.pending.lock().expect("usage mutex");
        if pending.is_empty() {
            return Ok(());
        }
        store::transaction(db, |tx| {
            for (hour, counts) in pending.iter() {
                let key = format!("usage:{hour}");
                let mut merged = tx
                    .query_row("select value from meta where key=?", [&key], |r| {
                        r.get::<_, String>(0)
                    })
                    .ok()
                    .and_then(|s| serde_json::from_str::<BTreeMap<String, i64>>(&s).ok())
                    .unwrap_or_default();
                for (class, count) in counts {
                    *merged.entry(class.clone()).or_default() += count;
                }
                tx.execute("insert into meta(key,value) values(?,?) on conflict(key) do update set value=excluded.value",
                    taskr_core::db::params![key, compact_json(&serde_json::to_value(merged)?)?])?;
            }
            let mut query = tx.prepare("select key from meta where key like 'usage:%'")?;
            let keys = query
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            for key in keys {
                if parse_hour(&key).is_some_and(|t| t < at - Duration::days(30)) {
                    tx.execute("delete from meta where key=?", [&key])?;
                }
            }
            Ok(())
        })?;
        pending.clear();
        Ok(())
    }
}

fn parse_hour(key: &str) -> Option<OffsetDateTime> {
    let raw = key.strip_prefix("usage:")?;
    if raw.len() != 13 {
        return None;
    }
    OffsetDateTime::parse(
        &format!("{raw}:00:00Z"),
        &time::format_description::well_known::Rfc3339,
    )
    .ok()
}

pub(crate) fn status(db: &Connection, at: OffsetDateTime) -> store::Result<Value> {
    let empty = || {
        CLASSES
            .iter()
            .map(|s| (s.to_string(), 0_i64))
            .collect::<BTreeMap<_, _>>()
    };
    let mut h24 = empty();
    let mut d7 = empty();
    let base = at
        .to_offset(time::UtcOffset::UTC)
        .replace_minute(0)
        .expect("minute")
        .replace_second(0)
        .expect("second")
        .replace_nanosecond(0)
        .expect("nanos");
    let mut query = db.prepare("select key,value from meta where key like 'usage:%'")?;
    for row in query.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
        let (key, raw) = row?;
        let Some(hour) = parse_hour(&key).filter(|t| *t >= base - Duration::hours(167)) else {
            continue;
        };
        let counts: BTreeMap<String, i64> =
            serde_json::from_str(&raw).map_err(|e| store::Error {
                code: taskr_core::ExitCode::Database,
                message: format!("decode {key}: {e}"),
            })?;
        for class in CLASSES {
            let count = counts.get(class).copied().unwrap_or(0);
            *d7.get_mut(class).expect("class") += count;
            if hour >= base - Duration::hours(23) {
                *h24.get_mut(class).expect("class") += count;
            }
        }
    }
    let minutes = |counts: &BTreeMap<String, i64>| {
        ((counts["state_loopback"] + counts["state_tailnet"]) as f64 / 2.).round() / 10.
    };
    Ok(
        json!({"visible_minutes_h24":minutes(&h24),"visible_minutes_d7":minutes(&d7),"h24":h24,"d7":d7}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn usage_flush_and_windows() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch("create table meta(key text primary key,value text)")
            .unwrap();
        let at = OffsetDateTime::parse(
            "2026-10-08T12:30:00Z",
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap();
        db.execute(
            "insert into meta values('usage:2026-10-07T12','{\"page\":3,\"state_tailnet\":20}')",
            [],
        )
        .unwrap();
        db.execute(
            "insert into meta values('usage:2026-08-01T12','{\"page\":99}')",
            [],
        )
        .unwrap();
        let usage = Usage::default();
        usage.record(200, at);
        usage.record(403, at);
        usage.record(421, at);
        usage.flush(&mut db, at).unwrap();
        usage.flush(&mut db, at).unwrap();
        let result = status(&db, at).unwrap();
        assert_eq!(result["h24"]["other"], 1);
        assert_eq!(result["h24"]["refused"], 2);
        assert_eq!(result["h24"]["page"], 0);
        assert_eq!(result["d7"]["page"], 3);
        assert_eq!(result["visible_minutes_d7"], 1.0);
        assert_eq!(
            db.query_row("select count(*) from meta", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
    }
}
