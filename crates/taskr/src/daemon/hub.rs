use super::*;
use std::{collections::BTreeSet, net::TcpListener};
use tokio::sync::watch;
fn clock() -> time::OffsetDateTime {
    taskr_core::frozen_now().expect("clock")
}
fn cfg(
    listeners: Vec<TcpListener>,
    path: PathBuf,
    home: PathBuf,
    identity: Option<crate::hub::HubIdentity>,
    hosts: BTreeSet<String>,
    tailscale_bin: PathBuf,
) -> crate::hub::HubConfig {
    crate::hub::HubConfig {
        listeners,
        db_path: path,
        clock,
        identity,
        hosts,
        home,
        tailscale_bin,
    }
}
fn loopback_cfg(listener: TcpListener, path: &Path, home: &Path) -> crate::hub::HubConfig {
    let port = listener.local_addr().expect("bound listener").port();
    let hosts = [
        format!("127.0.0.1:{port}"),
        format!("[::1]:{port}"),
        format!("localhost:{port}"),
    ]
    .into_iter()
    .collect();
    cfg(
        vec![listener],
        path.into(),
        home.into(),
        None,
        hosts,
        find_bin("tailscale").unwrap_or_else(|| PathBuf::from("tailscale")),
    )
}
async fn serve(
    cfg: crate::hub::HubConfig,
    mut stopped: watch::Receiver<bool>,
    log: Arc<Log>,
    key: Option<&'static str>,
) {
    let path = cfg.db_path.clone();
    if let Err(e) = crate::hub::serve(cfg, async move {
        if !*stopped.borrow() {
            let _ = stopped.changed().await;
        }
    })
    .await
    {
        log.line(&format!("hub serve failed: {e}"));
    }
    if let Some(key) = key {
        let _ = tokio::task::spawn_blocking(move || {
            if let Ok(db) = db::open(&path) {
                let _ = db.execute("delete from meta where key=?", [key]);
            }
        })
        .await;
    }
}
pub(super) fn start(
    rt: &tokio::runtime::Runtime,
    db: &db::Connection,
    dir: &Path,
    stay: bool,
    stopped: watch::Receiver<bool>,
    first: &mut Value,
    log: Arc<Log>,
) -> Result<Option<tokio::task::JoinHandle<()>>> {
    let config = match status::config(dir) {
        Ok(config) => config,
        Err(e) => {
            log.line(&format!("dashboard: {}; not serving", e.message));
            return Ok(None);
        }
    };
    if config.addr.is_empty() {
        log.line("dashboard: off");
        return Ok(None);
    }
    let home = PathBuf::from(store::env("HOME"));
    let path = db::path().map_err(store::usage)?;
    let listener = TcpListener::bind(&config.addr);
    let mut port = config
        .addr
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse::<u16>().ok())
        .unwrap_or(0);
    let mut loopback = None;
    match listener {
        Ok(listener) => {
            let addr = listener.local_addr().map_err(database)?;
            port = addr.port();
            let url = format!("http://{addr}/");
            set_meta(db, "dashboard_url", &url)?;
            first["dashboard_url"] = json!(url);
            loopback = Some(loopback_cfg(listener, &path, &home));
        }
        Err(e) => {
            log.line(&format!(
                "dashboard: listen {} failed: {e}; the event bridge carries on without it",
                config.addr
            ));
            if !stay && !config.tailnet {
                return Ok(None);
            }
        }
    }
    // Discovery remains net-owned. Tailnet retries never delay loopback or observations.
    let mut initial = if config.tailnet {
        match crate::net::hub_discovery(port) {
            Ok(d) => Some(d),
            Err(e) => {
                log.line(&format!(
                    "dashboard: tailnet unavailable ({e}); serving loopback only; retrying"
                ));
                None
            }
        }
    } else {
        None
    };
    let mut initial_bound = Vec::new();
    if let Some(discovery) = initial.as_ref() {
        for addr in &discovery.addresses {
            if let Ok(listener) = TcpListener::bind(addr) {
                initial_bound.push(listener);
            }
        }
        if !initial_bound.is_empty() {
            set_meta(db, "hub_tailnet_url", &discovery.url)?;
            first["tailnet_url"] = json!(discovery.url);
        }
    }
    Ok(Some(rt.spawn(async move{
        let mut tasks=tokio::task::JoinSet::new();let mut stopped=stopped;
        if let Some(cfg)=loopback{tasks.spawn(serve(cfg,stopped.clone(),log.clone(),Some("dashboard_url")));}
        else if stay{let path=path.clone();let home=home.clone();let addr=config.addr.clone();let log=log.clone();let mut stop=stopped.clone();tasks.spawn(async move{loop{tokio::select!{_=stop.changed()=>return,_=tokio::time::sleep(Duration::from_secs(2))=>{}}match TcpListener::bind(&addr){Ok(listener)=>{let url=format!("http://{}/",listener.local_addr().unwrap());let meta_path=path.clone();let _=tokio::task::spawn_blocking(move||{if let Ok(db)=db::open(&meta_path){let _=set_meta(&db,"dashboard_url",&url);}}).await;serve(loopback_cfg(listener,&path,&home),stop,log,Some("dashboard_url")).await;return},Err(e)=>log.limited("loopback-retry",Duration::from_secs(60),&format!("dashboard: listen {addr} failed: {e}; retrying"))}}});}
        if config.tailnet{
            let path=path.clone();let home=home.clone();let log=log.clone();let mut stop=stopped.clone();
            tasks.spawn(async move{
                let mut servers=tokio::task::JoinSet::new();let mut bound=BTreeSet::new();let mut delay=Duration::from_secs(2);
                loop{
                    if *stop.borrow(){break}
                    if initial.is_none(){initial=tokio::task::spawn_blocking(move||crate::net::hub_discovery(port)).await.ok().and_then(std::result::Result::ok);}
                    if let Some(discovery)=initial.as_ref(){
                        let mut listeners=std::mem::take(&mut initial_bound);
                        for addr in &discovery.addresses{if !bound.contains(addr)&&!listeners.iter().any(|l|l.local_addr().ok()==Some(*addr))&&let Ok(listener)=TcpListener::bind(addr){listeners.push(listener);}}
                        if !listeners.is_empty(){for listener in &listeners{bound.insert(listener.local_addr().unwrap());}let url=discovery.url.clone();let meta_path=path.clone();let _=tokio::task::spawn_blocking(move||{if let Ok(db)=db::open(&meta_path){let _=set_meta(&db,"hub_tailnet_url",&url);}}).await;servers.spawn(serve(cfg(listeners,path.clone(),home.clone(),Some(discovery.identity.clone()),discovery.hosts.clone(),discovery.tailscale_bin.clone()),stop.clone(),log.clone(),None));}
                        if discovery.addresses.iter().all(|a|bound.contains(a)){let _=stop.changed().await;break}
                    }
                    tokio::select!{_=stop.changed()=>break,_=tokio::time::sleep(delay)=>{}}delay=(delay*2).min(Duration::from_secs(60));
                }
                while servers.join_next().await.is_some(){}
            });
        }
        if !*stopped.borrow(){let _=stopped.changed().await;}
        while tasks.join_next().await.is_some(){}
    })))
}
