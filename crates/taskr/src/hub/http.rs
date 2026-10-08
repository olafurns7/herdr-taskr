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
}
struct Socket {
    stream: TcpStream,
    phase: Arc<Mutex<Phase>>,
    timer: Pin<Box<Sleep>>,
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
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let result = Pin::new(&mut self.stream).poll_flush(cx);
        if matches!(result, Poll::Ready(Ok(()))) {
            let mut phase = self.phase.lock().expect("phase");
            if phase.reply_ready {
                phase.reply_ready = false;
                phase.idle = true;
                phase.deadline = Some(Instant::now() + Duration::from_secs(60));
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
) -> io::Result<()> {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            _=shutdown.changed()=>break,
            accepted=listener.accept()=>{
                let(stream,peer)=accepted?;
                let phase=Arc::new(Mutex::new(Phase{deadline:Some(Instant::now()+Duration::from_secs(5)),idle:false,reply_ready:false}));
                let socket=Socket{stream,phase:phase.clone(),timer:Box::pin(tokio::time::sleep(Duration::from_secs(5)))};
                let service=TowerToHyperService::new(router.clone());
                let service=hyper::service::service_fn(move |mut req| {
                    req.extensions_mut().insert(ConnectInfo(peer));
                    {let mut state=phase.lock().expect("phase");state.deadline=None;state.idle=false;state.reply_ready=false;}
                    let phase=phase.clone();let future=hyper::service::Service::call(&service,req);
                    async move {let response=future.await;phase.lock().expect("phase").reply_ready=true;response}
                });
                let mut stopped=shutdown.clone();
                connections.spawn(async move {
                    let mut builder=hyper::server::conn::http1::Builder::new();
                    builder.header_read_timeout(None).max_header_size(16<<10).max_headers(4096);
                    let connection=builder.serve_connection(TokioIo::new(socket),service);
                    tokio::pin!(connection);
                    tokio::select! {
                        _=&mut connection=>{},
                        _=stopped.changed()=>{connection.as_mut().graceful_shutdown();let _=connection.await;}
                    }
                });
            }
            _=connections.join_next(),if !connections.is_empty()=>{}
        }
    }
    while connections.join_next().await.is_some() {}
    Ok(())
}
