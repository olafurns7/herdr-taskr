//! Notification-only SSE downlink; snapshots still use admitted RPC reads.
use super::*;
use axum::body::{Bytes, HttpBody};
use hyper::body::Frame;
use std::{
    convert::Infallible,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, broadcast};

const CAP: usize = 32;
const QUEUE: usize = 16;
#[derive(Clone, Debug)]
struct Change {
    rev: i64,
    seq: u64,
    version: i64,
    hosts: Vec<(String, String)>,
    reset: bool,
}
struct Inner {
    send: broadcast::Sender<Change>,
    current: Change,
    running: bool,
    failed: bool,
}
pub(super) struct Events {
    epoch: String,
    inner: StdMutex<Inner>,
    slots: Arc<Semaphore>,
    check: Notify,
}
impl Events {
    pub(super) fn new() -> Self {
        let (send, _) = broadcast::channel(QUEUE);
        Self {
            epoch: {
                static EPOCH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
                EPOCH
                    .get_or_init(|| {
                        format!(
                            "{}-{}",
                            std::process::id(),
                            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
                        )
                    })
                    .clone()
            },
            inner: StdMutex::new(Inner {
                send,
                current: Change {
                    rev: 0,
                    seq: 0,
                    version: 0,
                    hosts: Vec::new(),
                    reset: false,
                },
                running: false,
                failed: false,
            }),
            slots: Arc::new(Semaphore::new(CAP)),
            check: Notify::new(),
        }
    }
    pub(super) fn shared(path: &std::path::Path) -> Arc<Self> {
        // Loopback and tailnet listeners are separate Hub instances for one ledger.
        static WATCHERS: std::sync::OnceLock<StdMutex<BTreeMap<PathBuf, std::sync::Weak<Events>>>> =
            std::sync::OnceLock::new();
        let mut watchers = WATCHERS
            .get_or_init(StdMutex::default)
            .lock()
            .expect("event watchers");
        if let Some(events) = watchers.get(path).and_then(std::sync::Weak::upgrade) {
            return events;
        }
        watchers.retain(|_, events| events.strong_count() > 0);
        let events = Arc::new(Self::new());
        watchers.insert(path.into(), Arc::downgrade(&events));
        events
    }
    pub(super) fn check(&self) {
        if self.slots.available_permits() < CAP {
            self.check.notify_one();
        }
    }
}
fn read(db: &db::Connection) -> Result<Change, String> {
    // max(id) uses the events rowid index; host metadata uses its primary-key range.
    let rev = db
        .query_row("select coalesce(max(id),0) from events", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    let mut q = db.prepare("select key,coalesce(value,'') from meta where (key >= 'daemon_heartbeat' and key < 'daemon_heartbeau') or key='lead_listed_at' order by key").map_err(|e| e.to_string())?;
    let hosts = q
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    let version = db
        .query_row("pragma data_version", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    Ok(Change {
        rev,
        seq: 0,
        version,
        hosts,
        reset: false,
    })
}
fn next_seq() -> u64 {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
}
async fn poll(events: Arc<Events>, db: db::Connection) {
    let mut db = db;
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = events.check.notified() => {},
            _ = tick.tick() => {},
        }
        {
            let mut inner = events.inner.lock().expect("events");
            if inner.send.receiver_count() == 0 {
                inner.running = false;
                return;
            }
        }
        let result = tokio::task::spawn_blocking(move || {
            let result = read(&db);
            (db, result)
        })
        .await;
        let Ok((returned, change)) = result else {
            break;
        };
        db = returned;
        let mut inner = events.inner.lock().expect("events");
        match change {
            Ok(mut change) => {
                if change.rev != inner.current.rev
                    || change.hosts != inner.current.hosts
                    || change.version != inner.current.version
                    || inner.failed
                {
                    change.reset = change.rev < inner.current.rev
                        || change.rev > inner.current.rev.saturating_add(1);
                    change.seq = next_seq();
                    inner.failed = false;
                    inner.current = change.clone();
                    let _ = inner.send.send(change);
                }
            }
            Err(_) if !inner.failed => {
                inner.failed = true;
                let mut change = inner.current.clone();
                change.reset = true;
                change.seq = next_seq();
                inner.current.seq = change.seq;
                let _ = inner.send.send(change);
            }
            Err(_) => {}
        }
    }
    events.inner.lock().expect("events").running = false;
}

pub(super) async fn handle(hub: &Arc<Hub>, req: Request<Body>) -> Response {
    // Local admission exposes only epoch/revision/kinds and sequence, never ledger contents.
    // Remote identity admission is the same as RPC and happens in the parent route.
    let (parts, _) = req.into_parts();
    let header = |name: &str| {
        parts
            .headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    };
    if parts.method != axum::http::Method::GET {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            [("allow", "GET")],
            "Method Not Allowed\n",
        )
            .into_response();
    }
    if parts.headers.contains_key("origin") || parts.headers.contains_key("sec-fetch-site") {
        return http_error(StatusCode::FORBIDDEN, "browser requests are refused");
    }
    if header("x-taskr-rpc") != "1" {
        return http_error(StatusCode::BAD_REQUEST, "X-Taskr-RPC: 1 is required");
    }
    let Ok(permit) = hub.events.slots.clone().try_acquire_owned() else {
        return http_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "event subscriber limit reached",
        );
    };
    let path = hub.cfg.db_path.clone();
    let opened = tokio::task::spawn_blocking(move || {
        let db = db::open_migrated(&path)?;
        db.busy_timeout(Duration::from_millis(100))
            .map_err(|e| e.to_string())?;
        let current = read(&db)?;
        Ok::<_, String>((db, current))
    })
    .await;
    let Ok(Ok((db, mut current))) = opened else {
        return http_error(StatusCode::SERVICE_UNAVAILABLE, "event ledger unavailable");
    };
    let receiver;
    {
        let mut inner = hub.events.inner.lock().expect("events");
        // The polling read may have completed more recently than this admission read.
        if inner.running {
            current = inner.current.clone();
        } else {
            current.seq = next_seq();
            inner.current = current.clone();
        }
        receiver = inner.send.subscribe();
        if !inner.running {
            inner.running = true;
            tokio::spawn(poll(hub.events.clone(), db));
        }
    }
    let last = header("last-event-id");
    if !last.is_empty() && last != format!("{}:{}:{}", hub.events.epoch, current.seq, current.rev) {
        current.reset = true;
    }
    let mut stopped = hub.shutdown.clone();
    let stop = Box::pin(async move {
        if !*stopped.borrow() {
            let _ = stopped.changed().await;
        }
    });
    let body = EventBody {
        hub: hub.clone(),
        _permit: permit,
        initial: Some(current),
        receive: receiving(receiver),
        keepalive: Box::pin(tokio::time::sleep(Duration::from_secs(15))),
        stop,
    };
    (
        [
            ("content-type", "text/event-stream"),
            ("cache-control", "no-store"),
            ("x-accel-buffering", "no"),
        ],
        Body::new(body),
    )
        .into_response()
}
type Received = (
    broadcast::Receiver<Change>,
    Result<Change, broadcast::error::RecvError>,
);
fn receiving(
    mut receiver: broadcast::Receiver<Change>,
) -> Pin<Box<dyn Future<Output = Received> + Send>> {
    Box::pin(async move {
        let result = receiver.recv().await;
        (receiver, result)
    })
}
struct EventBody {
    hub: Arc<Hub>,
    _permit: OwnedSemaphorePermit,
    initial: Option<Change>,
    receive: Pin<Box<dyn Future<Output = Received> + Send>>,
    keepalive: Pin<Box<tokio::time::Sleep>>,
    stop: Pin<Box<dyn Future<Output = ()> + Send>>,
}
impl EventBody {
    fn frame(&self, change: Change) -> Frame<Bytes> {
        let epoch = &self.hub.events.epoch;
        let rev = change.rev;
        let seq = change.seq;
        let kind = if change.reset { "reset" } else { "change" };
        Frame::data(Bytes::from(format!(
            "event: {kind}\nid: {epoch}:{seq}:{rev}\ndata: {}\n\n",
            json!({"epoch":epoch,"rev":rev,"seq":seq,"kinds":["events","hosts","tasks"]})
        )))
    }
}
impl HttpBody for EventBody {
    type Data = Bytes;
    type Error = Infallible;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        if self.stop.as_mut().poll(cx).is_ready() {
            return Poll::Ready(None);
        }
        if let Some(change) = self.initial.take() {
            return Poll::Ready(Some(Ok(self.frame(change))));
        }
        if let Poll::Ready((mut receiver, change)) = self.receive.as_mut().poll(cx) {
            let change = match change {
                Ok(change) => change,
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let inner = self.hub.events.inner.lock().expect("events");
                    receiver = inner.send.subscribe();
                    let mut change = inner.current.clone();
                    change.reset = true;
                    change
                }
                Err(broadcast::error::RecvError::Closed) => return Poll::Ready(None),
            };
            self.receive = receiving(receiver);
            self.keepalive
                .as_mut()
                .reset(tokio::time::Instant::now() + Duration::from_secs(15));
            return Poll::Ready(Some(Ok(self.frame(change))));
        }
        if self.keepalive.as_mut().poll(cx).is_ready() {
            self.keepalive
                .as_mut()
                .reset(tokio::time::Instant::now() + Duration::from_secs(15));
            return Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(
                b": keepalive\n\n",
            )))));
        }
        Poll::Pending
    }
}

#[cfg(test)]
#[path = "../../tests/events/body.rs"]
mod tests;
