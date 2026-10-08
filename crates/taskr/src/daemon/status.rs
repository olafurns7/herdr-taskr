use super::*;
use std::net::IpAddr;
#[derive(Default)]
pub(super) struct Config {
    pub addr: String,
    pub tailnet: bool,
}
pub(super) fn config(dir: &Path) -> Result<Config> {
    let path = dir.join("dashboard.addr");
    let raw = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(database(e)),
    };
    let raw = raw.lines().next().unwrap_or("").trim();
    if raw == "off" {
        return Ok(Config::default());
    }
    if raw.is_empty() {
        return Ok(Config {
            addr: "127.0.0.1:7788".into(),
            tailnet: false,
        });
    }
    let port_error = |port: &str| {
        database(format!(
            "{}: bad port {}",
            path.display(),
            taskr_core::goflag::quote(port)
        ))
    };
    if raw == "tailnet" || raw.starts_with("tailnet:") {
        let port = raw.strip_prefix("tailnet:").unwrap_or("7788");
        if port.parse::<u16>().is_err() {
            return Err(port_error(port));
        }
        return Ok(Config {
            addr: format!("127.0.0.1:{port}"),
            tailnet: true,
        });
    }
    let (host, port) = if let Some(raw) = raw.strip_prefix('[') {
        raw.split_once("]:")
    } else {
        raw.rsplit_once(':').filter(|(host, _)| !host.contains(':'))
    }
    .ok_or_else(|| {
        database(format!(
            "{}: {} is not host:port or tailnet",
            path.display(),
            taskr_core::goflag::quote(raw)
        ))
    })?;
    let ip: Option<IpAddr> = host.parse().ok();
    if !matches!(ip,Some(IpAddr::V4(ip)) if ip==std::net::Ipv4Addr::LOCALHOST)
        && !matches!(ip,Some(IpAddr::V6(ip)) if ip==std::net::Ipv6Addr::LOCALHOST)
    {
        return Err(database(format!(
            "{}: host {} refused: the dashboard binds only 127.0.0.1 or ::1",
            path.display(),
            taskr_core::goflag::quote(host)
        )));
    }
    if port.parse::<u16>().is_err() {
        return Err(port_error(port));
    }
    let ip = ip.unwrap();
    Ok(Config {
        addr: if ip.is_ipv6() {
            format!("[{ip}]:{port}")
        } else {
            format!("{ip}:{port}")
        },
        tailnet: false,
    })
}
pub(super) fn local(db: &db::Connection, dir: &Path, lock: &Path) -> Result<Value> {
    let mut out =
        json!({"ok":true,"daemon":"none","running":false,"stay":false,"supervised":false});
    if let Some(at) = meta(db, "daemon_heartbeat")? {
        let age = age_ms(&at);
        out["daemon"] = json!(if age < 30_000 { "fresh" } else { "stale" });
        if !at.is_empty() {
            out["heartbeat_at"] = json!(at);
            out["heartbeat_age_ms"] = json!(age);
        }
    }
    out["dashboard_usage"] = usage(db).unwrap_or_else(|e| json!({"error":e.message}));
    out["socket"] = json!(meta(db, "daemon_socket")?.unwrap_or_else(herdr::socket));
    let pid = lock_pid(lock);
    let running = identity::alive(pid);
    out["running"] = json!(running);
    if running {
        let rec = identity::running(db, dir, pid)?;
        out["pid"] = json!(pid);
        out["running_version"] = json!(rec.version);
        out["stale"] = json!(rec.version != VERSION);
        out["stay"] = json!(rec.stay);
        out["supervised"] = json!(rec.supervised);
        if rec.herdr_missing {
            out["herdr_missing"] = json!(true)
        }
        if !rec.started_at.is_empty() {
            out["started_at"] = json!(rec.started_at)
        }
    }
    let url = meta(db, "dashboard_url")?;
    let cfg = config(dir);
    if running && let Some(url) = url {
        out["dashboard"] = json!("up");
        out["dashboard_url"] = json!(url);
    } else {
        match &cfg {
            Err(e) => {
                out["dashboard"] = json!("refused");
                out["dashboard_error"] = json!(e.message);
            }
            Ok(cfg) if cfg.addr.is_empty() => out["dashboard"] = json!("off"),
            Ok(cfg) => {
                out["dashboard"] = json!("down");
                out["dashboard_url"] = json!(format!("http://{}/", cfg.addr));
            }
        }
    }
    let hub = fs::read_to_string(dir.join("hub.url")).unwrap_or_default();
    let hub = hub.lines().next().unwrap_or("").trim();
    if cfg.is_ok_and(|c| c.tailnet) {
        out["role"] = json!("hub");
        if running && let Some(url) = meta(db, "hub_tailnet_url")? {
            out["tailnet_url"] = json!(url)
        }
    } else if !hub.is_empty() {
        out["role"] = json!("peer");
        out["hub_url"] = json!(hub);
        for (key, meta_key) in [
            ("last_push_ok_at", "peer_last_push_ok_at"),
            ("last_push_error", "peer_last_push_error"),
        ] {
            if let Some(s) = meta(db, meta_key)? {
                out[key] = json!(s)
            }
        }
    } else {
        out["role"] = json!("local");
    }
    Ok(out)
}
fn age_ms(at: &str) -> i64 {
    let now = taskr_core::frozen_now().expect("clock");
    let then = store::parse_time(at).unwrap_or_else(|| {
        time::OffsetDateTime::from_unix_timestamp(-62135596800).expect("year one")
    });
    ((now - then)
        .whole_nanoseconds()
        .clamp(i128::from(i64::MIN), i128::from(i64::MAX))
        / 1_000_000) as i64
}
fn usage(db: &db::Connection) -> Result<Value> {
    crate::hub::usage::status(db, taskr_core::frozen_now().expect("clock"))
}
