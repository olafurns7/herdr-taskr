use super::{Error, Result, command, flag, spoolable};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{IpAddr, TcpStream, ToSocketAddrs},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[cfg(feature = "contract")]
pub fn fixture() -> bool {
    std::env::var("TASKR_CONTRACT_TAILNET").is_ok_and(|s| s == "1")
        && std::env::var("TASKR_CONTRACT_ORACLE").is_ok_and(|s| s == "1")
}
#[cfg(not(feature = "contract"))]
pub fn fixture() -> bool {
    false
}
fn tailnet(ip: IpAddr) -> bool {
    (match ip {
        IpAddr::V4(v) => v.octets()[0] == 100 && (64..128).contains(&v.octets()[1]),
        IpAddr::V6(v) => v.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }) || (fixture() && ip == IpAddr::V6(std::net::Ipv6Addr::LOCALHOST))
}
pub(super) fn tailscale_bin() -> Result<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let executable = |p: &std::path::Path| {
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    let path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|d| d.join("tailscale"))
        .find(|p| p.is_absolute() && executable(p));
    path.or_else(|| {
        [
            "/usr/bin/tailscale",
            "/usr/local/bin/tailscale",
            "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
        ]
        .into_iter()
        .map(std::path::PathBuf::from)
        .find(|p| executable(p))
    })
    .ok_or_else(|| {
        Error::transport(
            "tailscale not found on PATH or in the usual places",
            true,
            false,
        )
    })
}

fn ts_out(args: &[&str]) -> Result<Vec<u8>> {
    let mut child = Command::new(tailscale_bin()?)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| Error::transport(e.to_string(), true, false))?;
    let stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut b = Vec::new();
        stdout.take((4 << 20) + 1).read_to_end(&mut b).map(|_| b)
    });
    let deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|e| Error::transport(e.to_string(), true, false))?
        {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::transport(
                format!("tailscale {}: timed out after 3s", args[0]),
                true,
                false,
            ));
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    let b = reader
        .join()
        .map_err(|_| Error::transport("tailscale reader failed", true, false))?
        .map_err(|e| Error::transport(e.to_string(), true, false))?;
    if !status.success() {
        return Err(Error::transport(
            format!(
                "tailscale {}: exit status {}",
                args[0],
                status.code().unwrap_or(-1)
            ),
            true,
            false,
        ));
    }
    if b.len() > 4 << 20 {
        return Err(Error::transport(
            format!("tailscale {}: output too large", args[0]),
            true,
            false,
        ));
    }
    Ok(b)
}
fn ts(args: &[&str]) -> Result<Value> {
    serde_json::from_slice(&ts_out(args)?)
        .map_err(|e| Error::transport(format!("tailscale {}: {e}", args[0]), true, false))
}

pub(super) struct Identity {
    pub node_id: String,
    pub dns: String,
    pub short: String,
    pub suffix: String,
    pub login: String,
    pub ip6: Option<std::net::Ipv6Addr>,
}
fn identity() -> Result<Identity> {
    let st = ts(&["status", "--json"])?;
    let me = &st["Self"];
    if me["ID"].as_str().unwrap_or("").is_empty() || me["DNSName"].as_str().unwrap_or("").is_empty()
    {
        return Err(Error::transport(
            "tailscale status: no Self node (logged out?)",
            true,
            false,
        ));
    }
    let id = me["UserID"].as_i64().unwrap_or(0).to_string();
    let login = st["User"][&id]["LoginName"].as_str().unwrap_or("");
    if login.is_empty() {
        return Err(Error::transport(
            "tailscale status: Self user not in User",
            true,
            false,
        ));
    }
    let dns = me["DNSName"]
        .as_str()
        .unwrap()
        .trim_end_matches('.')
        .to_lowercase();
    let (short, fallback) = dns.split_once('.').unwrap_or((&dns, ""));
    let sfx = st["MagicDNSSuffix"]
        .as_str()
        .unwrap_or("")
        .trim_matches('.')
        .to_lowercase();
    let sfx = if sfx.is_empty() {
        fallback.to_string()
    } else {
        sfx
    };
    if short.is_empty() || sfx.is_empty() {
        return Err(Error::transport(
            format!("tailscale status: cannot read a MagicDNS name from {dns:?}"),
            true,
            false,
        ));
    }
    let ip6 = me["TailscaleIPs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str()?.parse::<std::net::Ipv6Addr>().ok())
        .find(|v| v.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0]);
    Ok(Identity {
        node_id: me["ID"].as_str().unwrap().into(),
        short: short.into(),
        dns,
        suffix: sfx,
        login: login.into(),
        ip6,
    })
}
pub(super) fn self_identity() -> Result<(IpAddr, Identity)> {
    let out = ts_out(&["ip", "-4"])?;
    let text = String::from_utf8_lossy(&out);
    let first = text.trim().lines().next().unwrap_or("");
    let ip = first
        .trim()
        .parse::<IpAddr>()
        .ok()
        .filter(|ip| {
            matches!(ip, IpAddr::V4(_)) && tailnet(*ip)
                || fixture() && *ip == IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
        })
        .ok_or_else(|| {
            Error::transport(
                format!(
                    "tailscale ip -4: {:?} is not a tailnet address",
                    first.chars().take(60).collect::<String>()
                ),
                true,
                false,
            )
        })?;
    Ok((ip, identity()?))
}

#[derive(Clone)]
pub struct Client {
    pub authority: String,
    pub login: String,
    pub short: String,
}
impl Client {
    pub fn new(raw: &str) -> Result<Self> {
        let bad = || {
            Error::usage(format!(
                "server.url refused: {raw:?} is not http://HOST:PORT"
            ))
        };
        let raw_route = raw.strip_suffix('#').unwrap_or(raw);
        let raw_route = raw_route.strip_suffix('?').unwrap_or(raw_route);
        let authority = raw_route.strip_prefix("http://").ok_or_else(bad)?;
        let authority = authority.strip_suffix('/').unwrap_or(authority);
        if authority.contains(['/', '?', '#', '@', '\\']) {
            return Err(bad());
        }
        let (host, port) = authority.rsplit_once(':').ok_or_else(|| {
            Error::usage(format!(
                "server.url refused: {raw:?} needs a host and a port"
            ))
        })?;
        let host = host.trim_start_matches('[').trim_end_matches(']');
        if host.is_empty() || port.is_empty() {
            return Err(Error::usage(format!(
                "server.url refused: {raw:?} needs a host and a port"
            )));
        }
        if port.parse::<u16>().is_err() {
            return Err(bad());
        }
        if let Ok(ip) = host.parse::<IpAddr>() {
            // Go's URL check accepts IPv4 only; its contract hook also admits ::1.
            if !matches!(ip, IpAddr::V4(_)) && !(fixture() && ip.is_loopback()) || !tailnet(ip) {
                return Err(Error::usage(format!(
                    "server.url refused: {host} is not a tailnet address (100.64.0.0/10)"
                )));
            }
        }
        let me = identity()?;
        let sfx = &me.suffix;
        if host.parse::<IpAddr>().is_err() {
            let name = host.trim_end_matches('.').to_lowercase();
            if !name.ends_with(&format!(".{sfx}"))
                || name.matches('.').count() != sfx.matches('.').count() + 1
            {
                return Err(Error::usage(format!(
                    "server.url refused: {host} is not a MagicDNS name under {sfx}"
                )));
            }
        }
        Ok(Self {
            authority: authority.into(),
            login: me.login,
            short: me.short,
        })
    }
    pub fn call(&self, request: &Value, timeout: Duration, fallback: bool) -> Result<Value> {
        let mut request = request.clone();
        loop {
            let argv: Vec<String> = serde_json::from_value(request["argv"].clone())
                .map_err(|e| Error::usage(e.to_string()))?;
            let addr = self
                .authority
                .to_socket_addrs()
                .map_err(|e| Error::transport(e.to_string(), true, false))?;
            let deadline = Instant::now() + timeout;
            let mut connected = None;
            let mut last = "no addresses".to_string();
            for addr in addr {
                match TcpStream::connect_timeout(&addr, timeout.min(Duration::from_secs(10))) {
                    Ok(s) => {
                        connected = Some(s);
                        break;
                    }
                    Err(e) => last = e.to_string(),
                }
            }
            let mut stream = connected.ok_or_else(|| Error::transport(last, true, false))?;
            let peer = stream
                .peer_addr()
                .map_err(|e| Error::transport(e.to_string(), true, false))?;
            if !tailnet(peer.ip()) {
                return Err(Error::transport(
                    format!("hub address {peer} is not on the tailnet"),
                    true,
                    false,
                ));
            }
            let who = if fixture() {
                format!("hub-{}", peer.ip())
            } else {
                peer.ip().to_string()
            };
            let lookup = || -> Result<Value> {
                let w: Value = serde_json::from_slice(&ts_out(&["whois", "--json", &who])?)
                    .map_err(|_| Error::transport(format!("whois {who}: bad JSON"), true, false))?;
                let object_or_null = |v: &Value| v.is_null() || v.is_object();
                let string_or_null = |v: &Value| v.is_null() || v.is_string();
                if !object_or_null(&w)
                    || !object_or_null(&w["Node"])
                    || !object_or_null(&w["UserProfile"])
                    || !string_or_null(&w["Node"]["StableID"])
                    || !string_or_null(&w["Node"]["Name"])
                    || !string_or_null(&w["UserProfile"]["LoginName"])
                    || (!w["Node"]["Tags"].is_null()
                        && !w["Node"]["Tags"]
                            .as_array()
                            .is_some_and(|tags| tags.iter().all(string_or_null)))
                {
                    return Err(Error::transport(
                        format!("whois {who}: bad JSON"),
                        true,
                        false,
                    ));
                }
                Ok(w)
            };
            let w = lookup().map_err(|mut e| {
                e.message = format!("hub not verified: {}", e.message);
                e
            })?;
            let node = &w["Node"];
            let name = node["Name"].as_str().unwrap_or("");
            let short = name
                .trim_end_matches('.')
                .split('.')
                .next()
                .unwrap_or("")
                .to_lowercase()
                .chars()
                .take(63)
                .collect::<String>();
            let refuse = if node["StableID"].as_str().unwrap_or("").is_empty()
                || name.is_empty()
                || w["UserProfile"].is_null()
            {
                Some(format!("whois {who}: incomplete answer"))
            } else if node["Tags"].as_array().is_some_and(|a| !a.is_empty()) {
                Some(format!("whois {who}: tagged node {short}"))
            } else if w["UserProfile"]["LoginName"].as_str() != Some(&self.login) {
                Some(format!("whois {who}: node {short} belongs to another user"))
            } else {
                None
            };
            if let Some(msg) = refuse {
                return Err(Error::transport(
                    format!("hub not verified: {msg}"),
                    true,
                    false,
                ));
            }
            let remaining = deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(1));
            stream
                .set_read_timeout(Some(remaining))
                .map_err(Error::io)?;
            stream
                .set_write_timeout(Some(remaining))
                .map_err(Error::io)?;
            let b = taskr_core::compact_json(&request).unwrap();
            write!(stream, "POST /api/rpc HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nX-Taskr-RPC: 1\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}", self.authority, b.len(), b)
                .map_err(|e| Error::transport(e.to_string(), true, true))?;
            let (status, body) = response(stream)?;
            let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            if status == 400 && fallback {
                let msg = parsed["error"].as_str().unwrap_or("");
                let fields = ["capabilities", "document", "queued_at", "queued_age_ms"];
                if let Some(field) = fields.into_iter().find(|f| {
                    request.get(*f).is_some() && msg.contains(&format!("unknown field \"{f}\""))
                }) {
                    request.as_object_mut().unwrap().remove(field);
                    if field == "capabilities" {
                        request.as_object_mut().unwrap().remove("document");
                    }
                    continue;
                }
            }
            if status != 200 {
                let text = String::from_utf8_lossy(&body);
                let msg = parsed["error"].as_str().unwrap_or(text.trim());
                let clean = msg.split_whitespace().collect::<Vec<_>>().join(" ");
                let msg = if clean.chars().count() > 200 {
                    format!("{}…", clean.chars().take(199).collect::<String>())
                } else {
                    clean
                };
                let msg = format!("server answered {status}: {msg}");
                return Err(if status == 403 {
                    Error::rejected(msg)
                } else if (400..500).contains(&status) {
                    Error::usage(msg)
                } else {
                    Error::transport(
                        msg,
                        status >= 500 && spoolable(&argv) && !reply_shape(&parsed),
                        true,
                    )
                });
            }
            if !reply_shape(&parsed) {
                return Err(Error::transport("reply is not JSON", false, true));
            }
            return Ok(parsed);
        }
    }
}
fn reply_shape(v: &Value) -> bool {
    v["exit"].is_i64() && v["stdout"].is_string() && v["stderr"].is_string()
}
fn response(stream: TcpStream) -> Result<(u16, Vec<u8>)> {
    let err = |s: String| Error::transport(format!("reply cut off: {s}"), true, true);
    let mut r = BufReader::new(stream);
    let mut line = String::new();
    r.by_ref()
        .take(65537)
        .read_line(&mut line)
        .map_err(|e| err(e.to_string()))?;
    let status = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| err("invalid HTTP status".into()))?;
    let err = |s: String| Error::transport(format!("reply cut off: {s}"), status == 200, true);
    let mut length = None;
    let mut chunked = false;
    let mut header_bytes = line.len();
    if header_bytes > 65536 {
        return Err(Error::transport("reply headers too large", false, true));
    }
    loop {
        line.clear();
        if r.by_ref()
            .take((65537 - header_bytes) as u64)
            .read_line(&mut line)
            .map_err(|e| err(e.to_string()))?
            == 0
        {
            return Err(err("EOF".into()));
        }
        header_bytes += line.len();
        if header_bytes > 65536 {
            return Err(Error::transport("reply headers too large", false, true));
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.eq_ignore_ascii_case("content-length") {
                length = Some(
                    v.trim()
                        .parse::<usize>()
                        .map_err(|_| err("invalid content length".into()))?,
                );
            }
            if k.eq_ignore_ascii_case("transfer-encoding") {
                chunked = v.trim().eq_ignore_ascii_case("chunked");
            }
        }
    }
    let cap = 64 << 20;
    let mut b = Vec::new();
    if chunked {
        loop {
            line.clear();
            r.by_ref()
                .take(4097)
                .read_line(&mut line)
                .map_err(|e| err(e.to_string()))?;
            if line.len() > 4096 {
                return Err(Error::transport(
                    "reply chunk header too large",
                    false,
                    true,
                ));
            }
            let n = usize::from_str_radix(line.trim().split(';').next().unwrap_or(""), 16)
                .map_err(|_| err("invalid chunk size".into()))?;
            if n == 0 {
                loop {
                    line.clear();
                    if r.by_ref()
                        .take(65537)
                        .read_line(&mut line)
                        .map_err(|e| err(e.to_string()))?
                        == 0
                    {
                        return Err(err("EOF".into()));
                    }
                    header_bytes += line.len();
                    if header_bytes > 65536 {
                        return Err(Error::transport("reply trailers too large", false, true));
                    }
                    if line == "\r\n" || line == "\n" {
                        break;
                    }
                    if !line.contains(':') {
                        return Err(err("invalid HTTP trailer".into()));
                    }
                }
                break;
            }
            if n > cap - b.len() {
                return Err(Error::transport("reply too large", false, true));
            }
            let start = b.len();
            b.resize(start + n, 0);
            r.read_exact(&mut b[start..])
                .map_err(|e| err(e.to_string()))?;
            let mut end = [0; 2];
            r.read_exact(&mut end).map_err(|e| err(e.to_string()))?;
            if end != *b"\r\n" {
                return Err(err("invalid chunk ending".into()));
            }
        }
    } else if let Some(n) = length {
        if n > cap {
            return Err(Error::transport("reply too large", false, true));
        }
        b.resize(n, 0);
        r.read_exact(&mut b).map_err(|e| err(e.to_string()))?;
    } else {
        r.take(cap as u64 + 1)
            .read_to_end(&mut b)
            .map_err(|e| err(e.to_string()))?;
    }
    if b.len() > cap {
        return Err(Error::transport("reply too large", false, true));
    }
    Ok((status, b))
}
pub fn budget(argv: &[String]) -> Duration {
    let (cmd, args) = command(argv);
    if super::help(args) {
        return Duration::from_secs(30);
    }
    let ms = |k, d| {
        flag(args, k)
            .and_then(|(v, _, _)| v.parse::<u64>().ok())
            .unwrap_or(d)
    };
    match cmd {
        "wait" => Duration::from_millis(ms("timeout", 540000)),
        "prompt" | "_prompt" => {
            Duration::from_secs(30)
                + if super::flag_true(args, "confirm") {
                    Duration::from_millis(ms("confirm-timeout", 60000))
                } else {
                    Duration::ZERO
                }
        }
        "answer" if super::flag_true(args, "prompt") => {
            Duration::from_secs(30)
                + if super::flag_true(args, "confirm") {
                    Duration::from_millis(ms("confirm-timeout", 60000))
                } else {
                    Duration::ZERO
                }
        }
        _ => Duration::from_secs(30),
    }
}
pub fn request(argv: &[String], cwd: &str, key: &str, env: Value, doc: Option<Value>) -> Value {
    let (cmd, args) = command(argv);
    let mut r = json!({"argv":argv,"cwd":cwd,"request_key":key,"env":env});
    if matches!(
        cmd,
        "new" | "prompt" | "_prompt" | "ready" | "done" | "fail" | "close" | "_doc"
    ) || (cmd == "doc"
        && args
            .first()
            .is_some_and(|s| matches!(s.as_str(), "set" | "backfill")))
    {
        r["capabilities"] = json!(["doc-upload"]);
    }
    if let Some(d) = doc {
        r["document"] = d;
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    fn reply(bytes: &'static [u8]) -> Result<(u16, Vec<u8>)> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let peer = std::thread::spawn(move || {
            let (mut peer, _) = listener.accept().unwrap();
            peer.write_all(bytes).unwrap();
        });
        let result = response(stream);
        peer.join().unwrap();
        result
    }
    #[test]
    fn go_command_budgets() {
        for (args, seconds) in [
            (vec!["note", "x"], 30),
            (vec!["wait"], 540),
            (vec!["wait", "--timeout=1000"], 1),
            (vec!["wait", "--timeout=0"], 0),
            (vec!["prompt", "1", "--text", "x"], 30),
            (vec!["prompt", "1", "--text", "x", "--confirm"], 90),
            (vec!["answer", "1", "yes", "--prompt", "--confirm"], 90),
            (vec!["wait", "--help"], 30),
        ] {
            assert_eq!(
                budget(&args.into_iter().map(String::from).collect::<Vec<_>>()),
                Duration::from_secs(seconds)
            );
        }
    }
    #[test]
    fn bounded_http_framing_and_cutoff_retry() {
        assert_eq!(reply(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;ext=yes\r\nde\r\n0\r\n\r\n").unwrap(),(200,b"abcde".to_vec()));
        assert_eq!(
            reply(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nabcde").unwrap(),
            (200, b"abcde".to_vec())
        );
        assert!(
            reply(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nx")
                .unwrap_err()
                .retryable
        );
        assert!(
            !reply(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 5\r\n\r\nx")
                .unwrap_err()
                .retryable
        );
        assert!(
            !reply(b"HTTP/1.1 200 OK\r\nContent-Length: 67108865\r\n\r\n")
                .unwrap_err()
                .retryable
        );
        assert!(!reply_shape(&json!({"exit":0,"stdout":""})));
        assert!(reply_shape(&json!({"exit":6,"stdout":"x","stderr":""})));
    }
}
