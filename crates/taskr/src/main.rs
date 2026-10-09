#[cfg(feature = "contract")]
use std::path::PathBuf;
mod cli;
mod daemon;
mod hub;
mod net;
mod read;
mod tmp;
mod write;
use taskr_core::ExitCode;
fn run(mut args: Vec<String>) -> ExitCode {
    let mut json = std::env::var("TASKR_FORMAT").is_ok_and(|v| v == "json");
    if let Some(code) = hub::child_dispatch(&mut args) {
        return code;
    }
    if let Some(code) = net::events::dispatch(json, &args) {
        return code;
    }
    if let Some(code) = net::route(json, &args) {
        return code;
    }
    if args.first().is_some_and(|v| v == "--json") {
        json = true;
        args.remove(0);
    }
    let cmd = args.first().map(String::as_str).unwrap_or("");
    if !args.is_empty() && cmd.is_empty() {
        return cli::unknown(json, cmd);
    }
    if cmd == "--request-key" || cmd.starts_with("--request-key=") {
        let message = "--request-key is only for client mode (a server.url without TASKR_DB)";
        let value = if json {
            serde_json::json!({"error":message,"kind":"usage"})
        } else {
            serde_json::json!({"err":message,"k":"usage"})
        };
        println!(
            "{}{}",
            if json { "" } else { "x1 2 " },
            taskr_core::compact_json(&value).unwrap()
        );
        return ExitCode::Usage;
    }
    #[cfg(feature = "contract")]
    if cmd == "--contract-migrate" && args.len() == 1 {
        let path = taskr_core::db::path()
            .map_err(anyhow::Error::msg)
            .and_then(|p: PathBuf| taskr_core::db::open(&p).map_err(anyhow::Error::msg));
        return match path {
            Ok(_) => ExitCode::Ok,
            Err(e) => {
                eprintln!("{e}");
                ExitCode::Database
            }
        };
    }
    if let Some(code) = daemon::dispatch(json, &args)
        .or_else(|| read::dispatch(json, &args))
        .or_else(|| tmp::dispatch(json, &args))
        .or_else(|| write::dispatch(json, &args))
    {
        return code;
    }
    if !cli::known(cmd) {
        return cli::unknown(json, cmd);
    }
    cli::error(
        json,
        cmd,
        &format!("not implemented: {cmd}"),
        ExitCode::NotImplemented,
    )
}
fn main() -> std::process::ExitCode {
    std::process::ExitCode::from(run(std::env::args_os()
        .skip(1)
        .map(|v| v.to_string_lossy().into_owned())
        .collect()) as u8)
}
