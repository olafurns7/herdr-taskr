use axum::{Router, extract::ConnectInfo};
use hyper_util::{rt::TokioIo, service::TowerToHyperService};
use std::{
    future::Future,
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::watch,
    task::JoinSet,
    time::{Instant, Sleep},
};

struct Phase {
    deadline: Option<Instant>,
    idle: bool,
    reply_ready: bool,
    read_started: Instant,
    write_deadline: Option<Instant>,
    streaming: bool,
}
struct Socket {
    stream: TcpStream,
    phase: Arc<Mutex<Phase>>,
    timer: Pin<Box<Sleep>>,
    write_timer: Pin<Box<Sleep>>,
    keepalive_set: bool,
}
impl Socket {
    fn check_write_timeout(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        if let Some(deadline) = self.phase.lock().expect("phase").write_deadline {
            self.write_timer.as_mut().reset(deadline);
            if self.write_timer.as_mut().poll(cx).is_ready() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "HTTP write timeout",
                ));
            }
        }
        Ok(())
    }
}
impl AsyncRead for Socket {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let deadline = self.phase.lock().expect("phase").deadline;
        if let Some(deadline) = deadline {
            self.timer.as_mut().reset(deadline);
            if self.timer.as_mut().poll(cx).is_ready() {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "HTTP read timeout",
                )));
            }
        }
        let before = buf.filled().len();
        let result = Pin::new(&mut self.stream).poll_read(cx, buf);
        if matches!(result, Poll::Ready(Ok(()))) && buf.filled().len() > before {
            let mut phase = self.phase.lock().expect("phase");
            if phase.idle {
                phase.idle = false;
                phase.read_started = Instant::now();
                phase.deadline = Some(Instant::now() + Duration::from_secs(5));
            }
        }
        result
    }
}
impl AsyncWrite for Socket {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if !self.keepalive_set && self.phase.lock().expect("phase").streaming {
            let result = streaming_keepalive(&self.stream);
            if let Err(error) = result {
                return Poll::Ready(Err(error));
            }
            self.keepalive_set = true;
        }
        {
            let mut phase = self.phase.lock().expect("phase");
            let seconds = if phase.streaming { 30 } else { 15 };
            phase
                .write_deadline
                .get_or_insert_with(|| Instant::now() + Duration::from_secs(seconds));
        }
        if let Err(error) = self.check_write_timeout(cx) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Err(error) = self.check_write_timeout(cx) {
            return Poll::Ready(Err(error));
        }
        let result = Pin::new(&mut self.stream).poll_flush(cx);
        if matches!(result, Poll::Ready(Ok(()))) {
            let mut phase = self.phase.lock().expect("phase");
            if phase.streaming {
                phase.write_deadline = None;
                phase.reply_ready = false;
            } else if phase.reply_ready {
                phase.reply_ready = false;
                phase.idle = true;
                phase.deadline = Some(Instant::now() + Duration::from_secs(60));
                phase.write_deadline = None;
            }
        }
        result
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

pub(super) async fn listen(
    listener: TcpListener,
    router: Router,
    mut shutdown: watch::Receiver<bool>,
    log: Arc<crate::daemon::Log>,
) -> io::Result<()> {
    let mut connections = JoinSet::new();
    let mut last_logged: Option<Instant> = None;
    let mut suppressed = 0_u64;
    loop {
        tokio::select! {
            _=shutdown.changed()=>break,
            accepted=listener.accept()=>{
                let(stream,peer)=match accepted {
                    Ok(accepted)=>accepted,
                    Err(error)=>{
                        if last_logged.is_none_or(|at|at.elapsed()>=Duration::from_secs(10)) {
                            log.line(&format!("HTTP accept error: {error} ({suppressed} similar suppressed)"));
                            last_logged=Some(Instant::now());suppressed=0;
                        }else {suppressed=suppressed.saturating_add(1);}
                        tokio::select! {_=shutdown.changed()=>break,_=tokio::time::sleep(Duration::from_millis(200))=>{}}
                        continue;
                    }
                };
                let phase=Arc::new(Mutex::new(Phase{deadline:Some(Instant::now()+Duration::from_secs(5)),idle:false,reply_ready:false,read_started:Instant::now(),write_deadline:None,streaming:false}));
                let socket=Socket{stream,phase:phase.clone(),timer:Box::pin(tokio::time::sleep(Duration::from_secs(5))),write_timer:Box::pin(tokio::time::sleep(Duration::from_secs(15))),keepalive_set:false};
                let service=TowerToHyperService::new(router.clone());
                let service=hyper::service::service_fn(move |mut req| {
                    req.extensions_mut().insert(ConnectInfo(peer));
                    {let mut state=phase.lock().expect("phase");req.extensions_mut().insert(ReadDeadline(state.read_started+Duration::from_secs(10)));state.deadline=None;state.idle=false;state.reply_ready=false;state.write_deadline=None;}
                    let phase=phase.clone();let future=hyper::service::Service::call(&service,req);
                    async move {
                        let response=future.await;
                        let streaming=response.as_ref().is_ok_and(|r|r.headers().get("content-type").is_some_and(|v|v=="text/event-stream"));
                        let mut state=phase.lock().expect("phase");
                        state.streaming=streaming;state.reply_ready=true;
                        response
                    }
                });
                let mut stopped=shutdown.clone();
                connections.spawn(async move {
                    let mut builder=hyper::server::conn::http1::Builder::new();
                    builder.header_read_timeout(None).max_header_size(16<<10).max_headers(4096);
                    let connection=builder.serve_connection(TokioIo::new(socket),service);
                    tokio::pin!(connection);
                    tokio::select! {
                        _=&mut connection=>{},
                        _=stopped.changed()=>{connection.as_mut().graceful_shutdown();let _=tokio::time::timeout(Duration::from_secs(2),connection).await;}
                    }
                });
            }
            _=connections.join_next(),if !connections.is_empty()=>{}
        }
    }
    while connections.join_next().await.is_some() {}
    Ok(())
}

#[derive(Clone, Copy)]
pub(super) struct ReadDeadline(pub Instant);

fn streaming_keepalive(stream: &TcpStream) -> io::Result<()> {
    use rustix::net::sockopt::*;
    set_socket_keepalive(stream, true)?;
    set_tcp_keepidle(stream, Duration::from_secs(40))?;
    set_tcp_keepintvl(stream, Duration::from_secs(10))?;
    set_tcp_keepcnt(stream, 2)?;
    #[cfg(target_os = "linux")]
    set_tcp_user_timeout(stream, 60_000)?;
    Ok(())
}

#[cfg(test)]
#[tokio::test]
async fn streaming_socket_times_out_dead_peers_in_about_a_minute() {
    use rustix::net::sockopt::*;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (stream, _) = listener.accept().await.unwrap();
    streaming_keepalive(&stream).unwrap();
    assert!(socket_keepalive(&stream).unwrap());
    assert_eq!(tcp_keepidle(&stream).unwrap(), Duration::from_secs(40));
    assert_eq!(tcp_keepintvl(&stream).unwrap(), Duration::from_secs(10));
    assert_eq!(tcp_keepcnt(&stream).unwrap(), 2);
    #[cfg(target_os = "linux")]
    assert_eq!(tcp_user_timeout(&stream).unwrap(), 60_000);
    drop(client);
}
