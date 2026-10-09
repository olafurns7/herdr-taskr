//! Local read commands. JSON values preserve Go's map sorting and numeric rules.
use crate::cli;
use serde_json::{Value, json};
use taskr_core::{
    ExitCode,
    db::{self, Connection, OptionalExtension, params, params_from_iter, rusqlite},
    goflag::{self, FlagSet},
};
mod campaign;
mod glance;
mod queries;
mod slotr;
type Result<T> = std::result::Result<T, Error>;
#[derive(Debug)]
struct Error {
    code: ExitCode,
    text: String,
}
impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self {
            code: ExitCode::Database,
            text: db::error_text(&e),
        }
    }
}
fn usage(text: impl Into<String>) -> Error {
    Error {
        code: ExitCode::Usage,
        text: text.into(),
    }
}
fn reject(text: impl Into<String>) -> Error {
    Error {
        code: ExitCode::Rejected,
        text: text.into(),
    }
}
fn open() -> Result<Connection> {
    let p = db::path().map_err(usage)?;
    let db = db::open(&p).map_err(|text| Error {
        code: ExitCode::Database,
        text,
    })?;
    if taskr_core::store::rpc_context().is_none() {
        db.busy_timeout(std::time::Duration::from_secs(30))?;
    }
    Ok(db)
}
fn id(s: &str, what: &str) -> Result<i64> {
    s.parse::<i64>().ok().filter(|n| *n > 0).ok_or_else(|| {
        usage(format!(
            "{what} must be a positive integer, got {}",
            goflag::quote(s)
        ))
    })
}
fn flags(cmd: &str, json: bool) -> FlagSet {
    let mut f = FlagSet::new(cmd, json);
    match cmd {
        "status" => {
            f.int("tree", 0, "only this task and its descendants").bool(
                "all",
                false,
                "include closed tasks",
            );
        }
        "asks" => {
            f.bool("open", false, "only unanswered asks")
                .int("tree", 0, "only asks from this task and its descendants")
                .bool("owner", false, "only owner asks")
                .bool(
                    "all",
                    false,
                    "include unanswered asks from closed askers or closed roots",
                )
                .int("limit", 0, "latest N answered asks, 0 for all (default 20)");
        }
        "log" => {
            f.bool("tree", false, "include descendants")
                .int("since", 0, "only events after this id")
                .int("before", 0, "only events before this id")
                .int("limit", 0, "newest N events, 0 for all (default 100)");
        }
        "notes" => {
            f.bool("owner", false, "only notes for the owner")
                .int("root", 0, "campaign root id")
                .string(
                    "since",
                    "48h",
                    "events after this event id or within this Go duration",
                )
                .int(
                    "limit",
                    50,
                    "newest N notes; 0 lifts the limit and byte cap",
                );
        }
        "search" => {
            f.int("root", 0, "campaign root id")
                .string("kind", "", "document kind or decision, ask, answer, note")
                .int("limit", 20, "maximum hits, 1 to 100 (default 20)")
                .bool("raw", false, "use FTS5 query syntax");
        }
        "doc ls" => {
            f.bool("tree", false, "include descendants")
                .string("kind", "", "document kind")
                .bool("versions", false, "include every version")
                .int("limit", 100, "newest N documents; 0 for all (default 100)");
        }
        "campaign" => {
            f.int("page", 1, "log page, newest first (100 events per page)")
                .bool(
                    "all",
                    false,
                    "include closed lanes and their unanswered asks",
                );
        }
        "glance" => {
            f.bool("watch", false, "live terminal view")
                .duration("every", 5_000_000_000, "refresh interval (1s to 5m)")
                .bool("brief", false, "print the glance once as plain text for the hub")
                .string(
                    "since",
                    "",
                    "with --brief: the header's cursor= (an event id) or a Go duration such as 30m; hides campaigns with nothing newer",
                );
        }
        _ => {}
    }
    f
}
pub fn dispatch(json: bool, args: &[String]) -> Option<ExitCode> {
    let cmd = args.first().map(String::as_str).unwrap_or("");
    if matches!(cmd, "" | "-h" | "--help") {
        eprintln!("{}", cli::USAGE);
        if json {
            println!("{{\"error\":\"no command\",\"kind\":\"usage\"}}");
        } else {
            println!("x1 2 {{\"err\":\"no command\",\"k\":\"usage\"}}");
        }
        return Some(ExitCode::Usage);
    }
    let (name, rest) = if cmd == "doc" {
        if args
            .get(1)
            .is_some_and(|s| matches!(s.as_str(), "get" | "ls"))
        {
            (format!("doc {}", args[1]), &args[2..])
        } else if args.get(1).is_none_or(|s| s.starts_with('-')) {
            let mut f = flags("doc", json);
            if let Err(e) = f.parse(&args[1..], 1, 1) {
                return Some(cli::error(f.json(), cmd, &e, ExitCode::Usage));
            }
            if f.help() {
                print!("{}", f.usage(&cli::usage_line("doc")));
                return Some(ExitCode::Ok);
            }
            return Some(cli::error(
                f.json(),
                cmd,
                "doc: put the subcommand before its flags",
                ExitCode::Usage,
            ));
        } else {
            return None;
        }
    } else {
        (cmd.to_string(), &args[1..])
    };
    if !matches!(
        name.as_str(),
        "status"
            | "asks"
            | "notes"
            | "log"
            | "search"
            | "glance"
            | "campaign"
            | "slotr"
            | "doc ls"
            | "doc get"
            | "help"
            | "version"
    ) {
        return None;
    }
    let mut f = flags(&name, json);
    let (min, max) = match name.as_str() {
        "search" | "log" | "doc ls" | "doc get" | "campaign" => (1, 1),
        "help" => (0, 1),
        _ => (0, 0),
    };
    if let Err(e) = f.parse(rest, min, max) {
        return Some(cli::error(
            f.json(),
            cmd,
            if cmd == "version" {
                "version: takes no arguments"
            } else {
                &e
            },
            ExitCode::Usage,
        ));
    }
    if f.help() {
        print!("{}", f.usage(&cli::usage_line(&name)));
        return Some(ExitCode::Ok);
    }
    let result = match name.as_str() {
        "status" => queries::status(&f),
        "asks" => queries::asks(&f),
        "log" => queries::log(&f),
        "search" => queries::search(&f),
        "notes" => queries::notes(&f),
        "doc ls" => queries::doc_ls(&f),
        "doc get" => queries::doc_get(&f),
        "glance" => glance::run(&f),
        "campaign" => campaign::run(&f),
        "slotr" => slotr::run(&f),
        "version" => {
            let version = option_env!("TASKR_VERSION").unwrap_or("dev");
            println!(
                "{}{{\"version\":{},\"ok\":true}}",
                if f.json() { "" } else { "j1 " },
                taskr_core::escape_json(&serde_json::to_string(version).unwrap())
            );
            Ok(())
        }
        "help" => help(&f),
        _ => unreachable!(),
    };
    Some(match result {
        Ok(()) => ExitCode::Ok,
        Err(e) if e.text.is_empty() => e.code,
        Err(e) => cli::error(f.json(), cmd, &e.text, e.code),
    })
}
fn help(f: &FlagSet) -> Result<()> {
    if f.positional.is_empty() {
        println!("{}", cli::USAGE);
        return Ok(());
    }
    let cmd = &f.positional[0];
    if !cli::known(cmd) {
        cli::unknown(f.json(), cmd);
        return Err(Error {
            code: ExitCode::Usage,
            text: String::new(),
        });
    }
    let args = vec![cmd.clone(), "--help".into()];
    let code = crate::net::route(f.json(), &args)
        .or_else(|| dispatch(f.json(), &args))
        .or_else(|| crate::write::dispatch(f.json(), &args))
        .or_else(|| crate::daemon::dispatch(f.json(), &args));
    if code.is_none() {
        return Err(Error {
            code: ExitCode::NotImplemented,
            text: format!("not implemented: help {cmd}"),
        });
    }
    Ok(())
}
