//! Verified notification stream. Ledger snapshots remain ordinary RPC calls.
use super::{Error, Result, rpc, state_dir};
use serde::Deserialize;
use std::{
    io::{self, BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpStream},
    time::Duration,
};
use taskr_core::{ExitCode, db};

#[derive(Debug, Deserialize)]
pub(crate) struct Event {
    pub epoch: String,
    pub rev: i64,
    pub seq: u64,
    #[serde(default)]
    pub kinds: Vec<String>,
    #[serde(skip)]
    pub reset: bool,
}
pub(crate) struct EventStream {
    reader: BufReader<Chunks>,
    last: Option<(String, i64, u64)>,
}
fn stream() -> Result<EventStream> {
    let remote = if std::env::var("TASKR_DB").is_ok_and(|s| !s.is_empty()) {
        None
    } else {
        match std::fs::read_to_string(state_dir()?.join("server.url")) {
            Ok(s) => Some(s.lines().next().unwrap_or("").trim().to_owned()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(Error::io(e)),
        }
    };
    let (mut socket, authority) = if let Some(raw) = remote {
        let client = rpc::Client::new(&raw)?;
        (
            client.connect_verified(Duration::from_secs(10))?,
            client.authority,
        )
    } else {
        // Local discovery must not create or migrate a ledger.
        let db = db::Connection::open_with_flags(
            db::path().map_err(Error::usage)?,
            db::rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|e| Error::usage(e.to_string()))?;
        let url: String = db
            .query_row(
                "select value from meta where key='dashboard_url'",
                [],
                |r| r.get(0),
            )
            .map_err(|_| Error::usage("no local hub URL; start the hub daemon"))?;
        let authority = url
            .strip_prefix("http://")
            .ok_or_else(|| Error::usage("local hub URL must be http://LOOPBACK:PORT"))?
            .trim_end_matches('/');
        let address: SocketAddr = authority
            .parse()
            .map_err(|_| Error::usage("local hub URL must use a literal loopback address"))?;
        if !address.ip().is_loopback() {
            return Err(Error::rejected("local hub URL is not loopback"));
        }
        (
            TcpStream::connect_timeout(&address, Duration::from_secs(10)).map_err(Error::io)?,
            authority.to_owned(),
        )
    };
    socket
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(Error::io)?;
    socket
        .set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(Error::io)?;
    write!(socket, "GET /api/events HTTP/1.1\r\nHost: {authority}\r\nX-Taskr-RPC: 1\r\nAccept: text/event-stream\r\nConnection: close\r\n\r\n").map_err(Error::io)?;
    let mut reader = BufReader::new(socket);
    let status = line(&mut reader, 8192).map_err(Error::io)?;
    let status = status
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| Error::transport("invalid events HTTP status", false, true))?;
    let mut bytes = 0;
    let mut chunked = false;
    let mut sse = false;
    loop {
        let header = line(&mut reader, 8192).map_err(Error::io)?;
        bytes += header.len();
        if bytes > 16384 {
            return Err(Error::transport(
                "events HTTP headers too large",
                false,
                true,
            ));
        }
        if header == "\r\n" || header == "\n" {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("transfer-encoding") {
                if !value.trim().eq_ignore_ascii_case("chunked") {
                    return Err(Error::transport(
                        "unsupported events transfer encoding",
                        false,
                        true,
                    ));
                }
                chunked = true;
            }
            if name.eq_ignore_ascii_case("content-type") {
                sse = value.trim().split(';').next() == Some("text/event-stream");
            }
        }
    }
    if status != 200 {
        return Err(if status == 403 {
            Error::rejected("event stream admission refused")
        } else {
            Error::transport(format!("events server answered {status}"), false, true)
        });
    }
    if !sse {
        return Err(Error::transport(
            "events response is not text/event-stream",
            false,
            true,
        ));
    }
    Ok(EventStream {
        reader: BufReader::new(Chunks {
            reader,
            chunked,
            remaining: 0,
            trailing: false,
            ended: false,
        }),
        last: None,
    })
}
pub(crate) fn open() -> std::result::Result<EventStream, (ExitCode, String)> {
    stream().map_err(|e| (e.code, e.message))
}
fn line(reader: &mut impl BufRead, cap: usize) -> io::Result<String> {
    let mut s = String::new();
    if reader.take((cap + 1) as u64).read_line(&mut s)? == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "event stream closed",
        ));
    }
    if s.len() > cap || !s.ends_with('\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "event stream line too large or truncated",
        ));
    }
    Ok(s)
}
struct Chunks {
    reader: BufReader<TcpStream>,
    chunked: bool,
    remaining: usize,
    trailing: bool,
    ended: bool,
}
impl Read for Chunks {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() || self.ended {
            return Ok(0);
        }
        if !self.chunked {
            return self.reader.read(out);
        }
        if self.remaining == 0 {
            if self.trailing {
                let mut crlf = [0; 2];
                self.reader.read_exact(&mut crlf)?;
                if crlf != *b"\r\n" {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid event chunk delimiter",
                    ));
                }
            }
            let header = line(&mut self.reader, 128)?;
            self.remaining =
                usize::from_str_radix(header.trim().split(';').next().unwrap_or(""), 16).map_err(
                    |_| io::Error::new(io::ErrorKind::InvalidData, "invalid event chunk size"),
                )?;
            if self.remaining > 1 << 20 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "event chunk too large",
                ));
            }
            if self.remaining == 0 {
                self.ended = true;
                return Ok(0);
            }
            self.trailing = true;
        }
        let size = out.len().min(self.remaining);
        let n = self.reader.read(&mut out[..size])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated event chunk",
            ));
        }
        self.remaining -= n;
        Ok(n)
    }
}
impl EventStream {
    pub(crate) fn next_event(&mut self) -> std::result::Result<Option<Event>, (ExitCode, String)> {
        let mut result = || -> Result<Option<Event>> {
            let mut data = String::new();
            let mut kind = String::new();
            let mut bytes = 0;
            loop {
                let text = line(&mut self.reader, 8192).map_err(Error::io)?;
                bytes += text.len();
                if bytes > 16384 {
                    return Err(Error::transport("event frame too large", false, true));
                }
                let text = text.trim_end_matches(['\r', '\n']);
                if text.is_empty() {
                    if data.is_empty() {
                        return Ok(None);
                    }
                    let mut event: Event = serde_json::from_str(&data).map_err(|e| {
                        Error::transport(format!("invalid event: {e}"), false, true)
                    })?;
                    if event.epoch.is_empty()
                        || event.epoch.len() > 128
                        || event.rev < 0
                        || !matches!(kind.as_str(), "change" | "reset")
                    {
                        return Err(Error::transport("invalid event fields", false, true));
                    }
                    event.reset = kind == "reset"
                        || self.last.as_ref().is_some_and(|(epoch, rev, seq)| {
                            epoch != &event.epoch
                                || event.rev < *rev
                                || event.rev > rev.saturating_add(1)
                                || event.seq < *seq
                                || event.seq > seq.saturating_add(1)
                        });
                    self.last = Some((event.epoch.clone(), event.rev, event.seq));
                    return Ok(Some(event));
                }
                if let Some(value) = text.strip_prefix("data:") {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(value.strip_prefix(' ').unwrap_or(value));
                }
                if let Some(value) = text.strip_prefix("event:") {
                    kind = value.trim().to_owned();
                }
            }
        };
        result().map_err(|e| (e.code, e.message))
    }
}
pub(crate) fn dispatch(json: bool, args: &[String]) -> Option<ExitCode> {
    if args.first().map(String::as_str) != Some("_events") {
        return None;
    }
    let mut flags = taskr_core::goflag::FlagSet::new("_events", json);
    if let Err(e) = flags.parse(&args[1..], 0, 0) {
        return Some(Error::usage(e).emit(flags.json(), "_events"));
    }
    if flags.help() {
        println!("Usage: taskr _events [--json]\nPrint admitted hub change/reset notifications.");
        return Some(ExitCode::Ok);
    }
    let run = || -> std::result::Result<(), (ExitCode, String)> {
        let mut events = open()?;
        let mut out = io::stdout().lock();
        let mut reset = false;
        loop {
            let event = match events.next_event() {
                Ok(event) => event,
                Err((_, text)) if text == "event stream closed" => {
                    events = open()?;
                    reset = true;
                    continue;
                }
                Err(error) => return Err(error),
            };
            let text = if let Some(event) = event {
                let kind = if event.reset || reset {
                    "reset"
                } else {
                    "change"
                };
                reset = false;
                if flags.json() {
                    serde_json::json!({"epoch":event.epoch,"rev":event.rev,"seq":event.seq,"kinds":event.kinds,"event":kind}).to_string()
                } else {
                    format!("{kind} {} {} {}", event.epoch, event.rev, event.seq)
                }
            } else if flags.json() {
                "{\"keepalive\":true}".into()
            } else {
                ": keepalive".into()
            };
            if let Err(e) = writeln!(out, "{text}").and_then(|()| out.flush()) {
                if e.kind() == io::ErrorKind::BrokenPipe {
                    return Ok(());
                }
                return Err((ExitCode::Transport, e.to_string()));
            }
        }
    };
    Some(match run() {
        Ok(()) => ExitCode::Ok,
        Err((code, e)) => Error::new(code, "events", e).emit(flags.json(), "_events"),
    })
}
