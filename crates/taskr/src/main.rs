use std::path::PathBuf;

mod net;
mod read;
mod write;
use taskr_core::ExitCode;

fn bool_flag(value: &str) -> Option<bool> {
    match value {
        "1" | "t" | "T" | "true" | "TRUE" | "True" => Some(true),
        "0" | "f" | "F" | "false" | "FALSE" | "False" => Some(false),
        _ => None,
    }
}

fn error(json: bool, cmd: &str, message: &str, kind: &str, code: ExitCode) -> ExitCode {
    let value = if json {
        serde_json::json!({"error":message,"kind":kind})
    } else {
        serde_json::json!({"err":message,"k":kind})
    };
    let body = taskr_core::compact_json(&value).expect("valid error JSON");
    if json {
        eprintln!("taskr {cmd}: {message}");
        println!("{body}");
    } else {
        println!("x1 {} {body}", code as u8);
    }
    code
}

fn migrate() -> anyhow::Result<()> {
    let path = match std::env::var("TASKR_DB") {
        Ok(path) if !path.is_empty() => PathBuf::from(path),
        _ => PathBuf::from(std::env::var("HOME")?).join(".local/state/taskr/taskr.db"),
    };
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    taskr_core::schema::open(&path)?;
    Ok(())
}

fn run(mut args: Vec<String>) -> ExitCode {
    let mut json = std::env::var("TASKR_FORMAT").is_ok_and(|v| v == "json");
    if args.first().is_some_and(|arg| arg == "--json") {
        json = true;
        args.remove(0);
    }
    let cmd = args.first().map(String::as_str).unwrap_or("");
    if cmd == "--contract-migrate" && args.len() == 1 {
        return match migrate() {
            Ok(()) => ExitCode::Ok,
            Err(err) => {
                eprintln!("{err}");
                ExitCode::Database
            }
        };
    }
    if cmd != "version" {
        if let Some(code) = net::route(json, &args)
            .or_else(|| read::dispatch(json, &args))
            .or_else(|| write::dispatch(json, &args))
        {
            return code;
        }
        return error(
            json,
            cmd,
            &format!("not implemented: {cmd}"),
            "not_implemented",
            ExitCode::NotImplemented,
        );
    }
    let initial_json = json;
    let mut help = false;
    let mut short_help = false;
    let mut positional = false;
    let mut end_flags = false;
    for arg in &args[1..] {
        if end_flags {
            positional = true;
            continue;
        }
        if arg == "--" {
            end_flags = true;
            continue;
        }
        if !arg.starts_with('-') || arg == "-" {
            positional = true;
            continue;
        }
        let flag = arg.strip_prefix("--").unwrap_or(&arg[1..]);
        let (name, val) = flag.split_once('=').unwrap_or((flag, "true"));
        match (name, bool_flag(val)) {
            ("json", Some(value)) => json = value,
            ("help", Some(value)) => help = value,
            ("h", Some(value)) => short_help = value,
            _ => {
                return error(
                    json,
                    cmd,
                    "version: takes no arguments",
                    "usage",
                    ExitCode::Usage,
                );
            }
        }
    }
    if help || short_help {
        print!(
            "info:         version | help [CMD]\n  -h\tprint command help\n  -help\n    \tprint command help\n  -json\n    \tlegacy JSON output"
        );
        if initial_json {
            print!(" (default true)");
        }
        println!();
        return ExitCode::Ok;
    }
    if positional {
        return error(
            json,
            cmd,
            "version: takes no arguments",
            "usage",
            ExitCode::Usage,
        );
    }
    let version = option_env!("TASKR_VERSION").unwrap_or("dev");
    let body = format!(
        "{{\"version\":{},\"ok\":true}}",
        taskr_core::escape_json(&serde_json::to_string(version).expect("string"))
    );
    if json {
        println!("{body}");
    } else {
        println!("j1 {body}");
    }
    ExitCode::Ok
}

fn main() -> std::process::ExitCode {
    std::process::ExitCode::from(run(std::env::args_os()
        .skip(1)
        .map(|s| s.to_string_lossy().into_owned())
        .collect()) as u8)
}
