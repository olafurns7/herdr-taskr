use super::protocol::{RpcRequest, command};
use std::{cell::RefCell, io::Read};
use taskr_core::ExitCode;
use taskr_core::{db, store};

thread_local! {
    static REQUEST: RefCell<Option<RpcRequest>> = const { RefCell::new(None) };
}

pub(crate) fn dispatch(args: &mut Vec<String>) -> Option<ExitCode> {
    #[cfg(feature = "contract")]
    if args.first().is_some_and(|s| s == "--contract-hub") {
        return Some(fixture_server(args));
    }
    if args.first().is_none_or(|s| s != "--hub-child") {
        return None;
    }
    args.remove(0);
    let context = store::RpcContext {
        caller: store::env("TASKR_RPC_CALLER"),
        cwd: store::env("TASKR_RPC_CWD"),
        doc_upload: store::env("TASKR_RPC_DOC_UPLOAD") == "1",
        upload_file: store::env("TASKR_RPC_UPLOAD_FILE").into(),
    };
    if context.caller.is_empty() {
        return Some(crate::cli::error(
            false,
            "rpc",
            "invalid internal RPC context",
            ExitCode::Usage,
        ));
    }
    let mut input = Vec::new();
    let parsed = std::io::stdin()
        .take((super::protocol::BODY_MAX + 1) as u64)
        .read_to_end(&mut input)
        .map_err(|e| e.to_string())
        .and_then(|_| serde_json::from_slice::<RpcRequest>(&input).map_err(|e| e.to_string()));
    let req = match parsed {
        Ok(req) if input.len() <= super::protocol::BODY_MAX => req,
        _ => {
            return Some(crate::cli::error(
                false,
                "rpc",
                "invalid internal RPC context",
                ExitCode::Usage,
            ));
        }
    };
    if !store::init_rpc_context(context) {
        return Some(crate::cli::error(
            false,
            "rpc",
            "invalid internal RPC context",
            ExitCode::Usage,
        ));
    }
    REQUEST.with(|r| r.replace(Some(req)));
    #[cfg(feature = "contract")]
    if args.first().is_some_and(|s| s == "--contract-env") {
        let mut value = ["PATH", "HERDR_SOCKET_PATH", "HOME", "TASKR_DB"]
            .into_iter()
            .map(|k| (k, store::env(k)))
            .collect::<std::collections::BTreeMap<_, _>>();
        value.insert(
            "TASKR_RPC_CALLER",
            store::caller_machine().unwrap_or_default(),
        );
        println!(
            "{}",
            taskr_core::compact_json(&serde_json::json!(value)).expect("JSON")
        );
        return Some(ExitCode::Ok);
    }
    let (name, tail) = command(args);
    if name == "_doc" {
        let json_mode =
            args.first().is_some_and(|s| s == "--json") || store::env("TASKR_FORMAT") == "json";
        let result = REQUEST.with(|r| {
            let r = r.borrow();
            let req = r.as_ref().expect("request");
            if !store::rpc_context().is_some_and(|context| context.doc_upload) {
                return Err(store::reject("_doc requires the doc-upload capability"));
            }
            match tail.first().map(String::as_str) {
                Some("put") => super::documents::put(req, &tail[1..]),
                Some("wanted") => super::documents::wanted(&tail[1..], json_mode),
                _ => Err(store::usage("_doc: expected put or wanted")),
            }
        });
        return Some(match result {
            Ok(value) => {
                crate::cli::emit(json_mode, &value, false);
                ExitCode::Ok
            }
            Err(e) => crate::cli::error(json_mode, name, &e.message, e.code),
        });
    }
    if matches!(name, "_hook" | "_prompt" | "_host") {
        let json_mode =
            args.first().is_some_and(|s| s == "--json") || store::env("TASKR_FORMAT") == "json";
        if name == "_hook" {
            REQUEST.with(|r| {
                let r = r.borrow();
                let _ = super::hidden::hook(tail, r.as_ref().expect("request"));
            });
            return Some(ExitCode::Ok);
        }
        let result = if name == "_prompt" {
            super::hidden::prompt(tail, json_mode)
        } else {
            super::hidden::host(tail, json_mode)
        };
        return Some(match result {
            Ok(mut value) => {
                if value.get("error").is_some() {
                    let code = if value["_exit"] == 6 {
                        ExitCode::Rejected
                    } else {
                        ExitCode::Transport
                    };
                    value.as_object_mut().expect("object").remove("_exit");
                    let message = value["error"].as_str().unwrap_or("").to_string();
                    crate::write::error(json_mode, name, store::Error { code, message }, value)
                } else {
                    crate::cli::emit(json_mode, &value, false);
                    ExitCode::Ok
                }
            }
            Err(e) => crate::write::error(json_mode, name, e, serde_json::json!({})),
        });
    }
    None
}

pub(super) fn open() -> store::Result<db::Connection> {
    db::open(&db::path().map_err(store::usage)?).map_err(|message| store::Error {
        code: ExitCode::Database,
        message,
    })
}

pub(crate) fn document_set_input(
    id: i64,
    kind: &str,
    name: &str,
    path: &str,
) -> store::Result<store::documents::Input> {
    REQUEST.with(|r| {
        let r = r.borrow();
        let payload = r.as_ref().and_then(|r| r.document.as_ref());
        let Some(p) = payload else {
            return Err(store::usage("file not found on the server host; in this release the file must be on that host's disk"));
        };
        if !store::rpc_context().is_some_and(|context| context.doc_upload) || p.task != id || p.kind != kind || p.name != name || p.path != path || p.event_id.is_some() || p.backfill || p.dry_run {
            return Err(store::reject("doc set body does not match its request"));
        }
        super::documents::input(p)
    })
}

#[cfg(feature = "contract")]
fn fixture_server(args: &[String]) -> ExitCode {
    use std::io::Write;
    let result = (|| -> anyhow::Result<()> {
        let listener = std::net::TcpListener::bind("[::1]:0")?;
        let address = listener.local_addr()?;
        let loopback = std::net::TcpListener::bind("127.0.0.1:0")?;
        let loopback_address = loopback.local_addr()?;
        let tailscale = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|p| p.join("tailscale"))
            .find(|p| p.is_file())
            .ok_or_else(|| anyhow::anyhow!("fixture tailscale missing"))?;
        let cfg = super::HubConfig {
            listeners: vec![listener, loopback],
            db_path: db::path().map_err(anyhow::Error::msg)?,
            clock: || taskr_core::frozen_now().expect("clock"),
            identity: Some(super::HubIdentity {
                node_id: args.get(1).cloned().unwrap_or_else(|| "hub-node".into()),
                machine: "hub".into(),
                login: "owner@example.com".into(),
            }),
            hosts: [address.to_string(), loopback_address.to_string()]
                .into_iter()
                .collect(),
            home: store::env("HOME").into(),
            tailscale_bin: tailscale,
        };
        db::open(&cfg.db_path).map_err(anyhow::Error::msg)?;
        println!("http://{address}\nhttp://{loopback_address}");
        std::io::stdout().flush()?;
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?
            .block_on(super::serve(cfg, async {
                let mut term =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("signal");
                tokio::select! { _ = term.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
            }))
    })();
    match result {
        Ok(()) => ExitCode::Ok,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::Database
        }
    }
}
