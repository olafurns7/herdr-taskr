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
        _permit: hub.events.slots.clone().acquire_owned().await.unwrap(),
        initial: None,
        receive: receiving(receiver),
        keepalive: Box::pin(tokio::time::sleep(Duration::from_secs(15))),
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
}

#[test]
fn one_watcher_and_cap_across_loopback_and_tailnet_hubs() {
    let path = std::path::Path::new("/synthetic/events-ledger");
    let local = Events::shared(path);
    let remote = Events::shared(path);
    assert!(Arc::ptr_eq(&local, &remote));
    assert!(!local.inner.lock().unwrap().running);
    let permit = local.slots.clone().try_acquire_owned().unwrap();
    assert_eq!(remote.slots.available_permits(), CAP - 1);
    drop(permit);
    assert_eq!(remote.slots.available_permits(), CAP);
}
