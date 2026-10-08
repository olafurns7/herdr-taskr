//! Deterministic slow-consumer check: exercise the bounded body queue directly.
use super::*;

#[tokio::test]
async fn overflow_resets_to_current_and_discards_old_notifications() {
    let (_, shutdown) = watch::channel(false);
    let hub = Arc::new(Hub {
        cfg: HubConfig {
            listeners: vec![],
            db_path: PathBuf::new(),
            clock: time::OffsetDateTime::now_utc,
            log: Arc::new(crate::daemon::Log::open(std::path::Path::new("/dev/null"))),
            identity: None,
            hosts: BTreeSet::new(),
            home: PathBuf::new(),
            tailscale_bin: PathBuf::new(),
        },
        whois: Mutex::default(),
        usage: usage::Usage::default(),
        events: Arc::new(Events::new()),
        shutdown,
        stored_requests: StdMutex::default(),
        reapers: Arc::default(),
    });
    let receiver = hub.events.inner.lock().unwrap().send.subscribe();
    let mut body = EventBody {
        hub: hub.clone(),
        _permit: hub
            .events
            .acquire("127.0.0.1".parse().unwrap(), true)
            .unwrap(),
        initial: None,
        receive: receiving(receiver),
        keepalive: Box::pin(tokio::time::sleep(Duration::from_secs(15))),
        expires: Box::pin(tokio::time::sleep(MAX_AGE)),
        stop: Box::pin(std::future::pending()),
    };
    for rev in 1..=100 {
        let mut inner = hub.events.inner.lock().unwrap();
        inner.current.rev = rev;
        inner.send.send(inner.current.clone()).unwrap();
    }
    let frame = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx))
        .await
        .unwrap()
        .unwrap()
        .into_data()
        .unwrap();
    let text = std::str::from_utf8(&frame).unwrap();
    assert!(text.starts_with("event: reset\n"), "{text}");
    assert!(text.contains("\"rev\":100"), "{text}");
    assert!(
        tokio::time::timeout(
            Duration::from_millis(10),
            std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx))
        )
        .await
        .is_err()
    );
    {
        let mut inner = hub.events.inner.lock().unwrap();
        inner.current.rev = 101;
        inner.send.send(inner.current.clone()).unwrap();
    }
    let frame = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx))
        .await
        .unwrap()
        .unwrap()
        .into_data()
        .unwrap();
    assert!(std::str::from_utf8(&frame).unwrap().contains("\"rev\":101"));
    assert_eq!(MAX_AGE, Duration::from_secs(600));
    body.expires.as_mut().reset(tokio::time::Instant::now());
    assert!(
        std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx))
            .await
            .is_none()
    );
    drop(body);
    assert_eq!(hub.events.local_slots.available_permits(), LOCAL_CAP);
}

#[test]
fn one_watcher_and_cap_across_loopback_and_tailnet_hubs() {
    let path = std::path::Path::new("/synthetic/events-ledger");
    let local = Events::shared(path);
    let remote = Events::shared(path);
    assert!(Arc::ptr_eq(&local, &remote));
    assert!(!local.inner.lock().unwrap().running);
    let ip = "127.0.0.1".parse().unwrap();
    let mut permits: Vec<_> = (0..IP_CAP)
        .map(|_| local.acquire(ip, true).unwrap())
        .collect();
    assert!(remote.acquire(ip, true).is_none());
    assert!(remote.acquire(ip, false).is_none());
    assert_eq!(remote.remote_slots.available_permits(), REMOTE_CAP);
    for _ in 0..IP_CAP {
        permits.push(local.acquire("127.0.0.2".parse().unwrap(), true).unwrap());
    }
    assert!(local.acquire("127.0.0.3".parse().unwrap(), true).is_none());
    let mut remote_permits = Vec::new();
    for n in 1..=6 {
        for _ in 0..IP_CAP {
            remote_permits.push(
                remote
                    .acquire(format!("192.0.2.{n}").parse().unwrap(), false)
                    .unwrap(),
            );
        }
    }
    assert_eq!(remote.remote_slots.available_permits(), 0);
    assert!(
        remote
            .acquire("192.0.2.7".parse().unwrap(), false)
            .is_none()
    );
    drop(permits);
    assert_eq!(local.local_slots.available_permits(), LOCAL_CAP);
    assert!(local.acquire(ip, true).is_some());
    drop(remote_permits);
    assert_eq!(remote.remote_slots.available_permits(), REMOTE_CAP);
    assert!(
        remote
            .acquire("192.0.2.1".parse().unwrap(), false)
            .is_some()
    );
}
