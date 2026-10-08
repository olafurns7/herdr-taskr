use super::*;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    sync::watch,
};
pub(super) enum Wake {
    Dirty,
    Attached,
    Reset,
    Gone,
}
pub(super) async fn subscribe(
    sock: String,
    stay: bool,
    mut panes: watch::Receiver<Vec<String>>,
    mut stop: watch::Receiver<bool>,
    send: std::sync::mpsc::SyncSender<Wake>,
    connected: Arc<AtomicBool>,
    log: Arc<Log>,
) {
    let mut attempts = 0u32;
    let mut attach = true;
    let mut last_error = None::<Instant>;
    loop {
        if *stop.borrow() {
            return;
        }
        if !stay && !Path::new(&sock).exists() {
            let _ = send.send(Wake::Gone);
            return;
        }
        let stream =
            tokio::select! {_ = stop.changed()=>return,result=UnixStream::connect(&sock)=>result};
        let mut acked = false;
        let mut resub = false;
        let mut stream_error = false;
        if let Ok(mut stream) = stream {
            let subscribed = panes.borrow_and_update().clone();
            let req = request(&subscribed);
            if stream.write_all(&req).await.is_ok() {
                let mut buf = [0; 32 << 10];
                let mut line = Vec::new();
                let mut long = false;
                'stream: loop {
                    let n = tokio::select! {
                        _=stop.changed()=>return,
                        changed=panes.changed()=>{
                            if changed.is_err(){return}
                            if *panes.borrow_and_update()!=subscribed{log.line(&format!("pane set changed; resubscribing with {} panes",panes.borrow().len()));resub=true;break 'stream}
                            continue
                        },
                        read=stream.read(&mut buf)=>match read {Ok(0)|Err(_)=>break 'stream,Ok(n)=>n}
                    };
                    if buf[..n].contains(&b'\n') {
                        let _ = send.try_send(Wake::Dirty);
                    }
                    for byte in &buf[..n] {
                        if *byte != b'\n' {
                            if !long && line.len() < 64 << 10 {
                                line.push(*byte);
                            } else {
                                long = true;
                                line.clear();
                            }
                            continue;
                        }
                        let reply = if !long {
                            serde_json::from_slice::<Value>(&line).ok()
                        } else {
                            None
                        };
                        if reply.is_none() {
                            stream_error = true;
                            log.limited(
                                "stream-malformed",
                                Duration::from_secs(10),
                                "malformed subscription message; resubscribing",
                            );
                            break 'stream;
                        }
                        let error = reply.as_ref().is_some_and(|v| v["error"].is_object());
                        if !acked {
                            acked = true;
                            if error {
                                stream_error = true;
                                let err = &reply.as_ref().unwrap()["error"];
                                log.line(&format!(
                                    "subscribe rejected: {} {}",
                                    err["code"].as_str().unwrap_or(""),
                                    tokens::truncate(err["message"].as_str().unwrap_or(""), 200)
                                ));
                            } else {
                                connected.store(true, Ordering::SeqCst);
                                log.line(&format!("subscribed with {} panes", subscribed.len()));
                                if attach {
                                    let _ = send.try_send(Wake::Attached);
                                }
                            }
                        } else if error {
                            stream_error = true;
                            resub =
                                last_error.is_none_or(|t| t.elapsed() >= Duration::from_secs(1));
                            last_error = Some(Instant::now());
                            log.limited(
                                "stream-error",
                                Duration::from_secs(10),
                                &format!(
                                    "stream error: {} {}; resubscribing",
                                    tokens::truncate(
                                        reply.as_ref().unwrap()["error"]["code"]
                                            .as_str()
                                            .unwrap_or(""),
                                        60
                                    ),
                                    tokens::truncate(
                                        reply.as_ref().unwrap()["error"]["message"]
                                            .as_str()
                                            .unwrap_or(""),
                                        200
                                    )
                                ),
                            );
                            break 'stream;
                        }
                        line.clear();
                        long = false;
                    }
                }
            }
        } else {
            log.limited(
                "connect",
                Duration::from_secs(if stay { 60 } else { 0 }),
                "connect failed",
            );
        }
        connected.store(false, Ordering::SeqCst);
        if !resub || stream_error {
            let _ = send.try_send(Wake::Reset);
        }
        if acked {
            attempts = 0;
        }
        if acked || !resub {
            attach = !resub;
        }
        if *stop.borrow() {
            return;
        }
        if !stay && !Path::new(&sock).exists() {
            let _ = send.send(Wake::Gone);
            return;
        }
        if resub {
            continue;
        }
        attempts = attempts.saturating_add(1);
        let delay = Duration::from_millis(500u64 << attempts.saturating_sub(1).min(6))
            .min(Duration::from_secs(if stay { 2 } else { 30 }));
        if !stay {
            log.line(&format!(
                "disconnected; reconnect in {}ms",
                delay.as_millis()
            ));
        }
        tokio::select! {_=stop.changed()=>return,_=tokio::time::sleep(delay)=>{}}
    }
}
pub(super) fn request(panes: &[String]) -> Vec<u8> {
    let mut subs: Vec<_> = ["pane.exited", "pane.closed", "pane.agent_detected"]
        .into_iter()
        .map(|kind| json!({"type":kind}))
        .collect();
    subs.extend(
        panes
            .iter()
            .map(|pane| json!({"type":"pane.agent_status_changed","pane_id":pane})),
    );
    let mut out = compact_json(
        &json!({"id":"taskr-daemon","method":"events.subscribe","params":{"subscriptions":subs}}),
    )
    .unwrap()
    .into_bytes();
    out.push(b'\n');
    out
}
