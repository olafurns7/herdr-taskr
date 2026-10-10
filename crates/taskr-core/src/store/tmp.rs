//! Last-reported host-local logical bytes, carried by ordinary ref events.
use super::*;

pub fn host(key: &str) -> Option<&str> {
    key.strip_prefix("tmp.bytes.").filter(|host| {
        !host.is_empty()
            && host.len() <= 63
            && host
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b))
    })
}

pub fn bytes(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

pub fn total(db: &Connection, root: i64) -> rusqlite::Result<Option<u64>> {
    let mut stmt = db.prepare("select json_extract(data,'$.key'),json_extract(data,'$.value') from events where id in (select max(id) from events where task_id=? and kind='ref' group by json_extract(data,'$.key'))")?;
    let mut total = None;
    for row in stmt.query_map([root], |r| {
        Ok((
            r.get::<_, rusqlite::types::Value>(0)?,
            r.get::<_, rusqlite::types::Value>(1)?,
        ))
    })? {
        let (rusqlite::types::Value::Text(key), rusqlite::types::Value::Text(value)) = row? else {
            continue;
        };
        if host(&key).is_some()
            && let Some(n) = bytes(&value)
        {
            let Some(sum) = total.unwrap_or(0_u64).checked_add(n) else {
                return Ok(None);
            };
            total = Some(sum);
        }
    }
    Ok(total)
}

pub fn compact(bytes: u64) -> String {
    let mut value = bytes as f64;
    let mut unit = "";
    for next in ["K", "M", "G", "T", "P", "E"] {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = next;
    }
    if unit.is_empty() {
        bytes.to_string()
    } else {
        format!("{value:.1}{unit}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn host_and_bytes_are_bounded_and_no_unknown_becomes_zero() {
        assert!(host(&format!("tmp.bytes.{}", "h".repeat(63))).is_some());
        for key in [
            "tmp.bytes.",
            "tmp.bytes.Host",
            "tmp.bytes.host/path",
            "tmp.bytes.this machine",
            "tmp.bytes.h…",
        ] {
            assert!(host(key).is_none());
        }
        assert!(host(&format!("tmp.bytes.{}", "h".repeat(64))).is_none());
        for value in ["", "-1", "+1", "1.2", " 1", "18446744073709551616"] {
            assert_eq!(bytes(value), None);
        }
        assert_eq!(bytes("0"), Some(0));
        assert_eq!(bytes("18446744073709551615"), Some(u64::MAX));
        assert_eq!(compact(0), "0");
        assert_eq!(compact(1288490189), "1.2G");
    }
}
