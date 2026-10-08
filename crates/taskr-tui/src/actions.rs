//! What a key can do outside the view: focus a Herdr pane, answer an ask, park a campaign,
//! copy text. The view decides and confirms; this module only runs the job.

use std::{
    env,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    time::Duration,
};

use crate::client;

#[derive(Debug, Clone, PartialEq)]
pub enum Job {
    /// Focus a pane on this host's Herdr server. It moves every client attached to it.
    Focus { pane: String },
    /// `taskr answer ASK TEXT`, with `--prompt` when the asker is not waiting.
    Answer {
        ask_id: i64,
        text: String,
        prompt: bool,
    },
    /// `taskr set ROOT glance.state=parked`, or `glance.state=` to unpark.
    Park { root: i64, name: String, park: bool },
}

impl Job {
    /// What the job does, for the question before it and the line after it.
    pub fn label(&self) -> String {
        match self {
            Job::Focus { pane } => format!("go to {pane}"),
            Job::Answer { ask_id, .. } => format!("answer {ask_id}"),
            Job::Park {
                name, park: true, ..
            } => format!("park {name}"),
            Job::Park { name, .. } => format!("unpark {name}"),
        }
    }

    /// The taskr arguments, for the jobs taskr runs.
    pub fn args(&self) -> Option<Vec<String>> {
        match self {
            Job::Focus { .. } => None,
            Job::Answer {
                ask_id,
                text,
                prompt,
            } => {
                let mut args = vec!["answer".into(), ask_id.to_string(), text.clone()];
                if *prompt {
                    args.push("--prompt".into());
                }
                Some(args)
            }
            Job::Park { root, park, .. } => {
                let state = if *park {
                    "glance.state=parked"
                } else {
                    "glance.state="
                };
                Some(vec!["set".into(), root.to_string(), state.into()])
            }
        }
    }

    /// Runs the job and waits for it. `taskr` is the command that reaches the ledger.
    pub fn run(&self, taskr: &[String]) -> Result<(), String> {
        match (self, self.args()) {
            (Job::Focus { pane }, _) => {
                let socket = env::var("HERDR_SOCKET_PATH").map_err(|_| "not in a Herdr pane")?;
                focus(&socket, pane)
            }
            (_, Some(args)) => {
                let command: Vec<String> = taskr.iter().cloned().chain(args).collect();
                client::output(&command, client::TIMEOUT).map(|_| ())
            }
            (_, None) => Ok(()),
        }
    }
}

/// The socket method `pane.focus`: workspace, tab and pane in one call, for any pane. The
/// herdr CLI has no form for it (`herdr pane focus` is directional), and the socket cannot
/// start a server the way the CLI can.
pub fn focus(socket: &str, pane: &str) -> Result<(), String> {
    let fail = |e: std::io::Error| format!("herdr: {e}");
    let mut stream = UnixStream::connect(socket).map_err(fail)?;
    let wait = Some(Duration::from_secs(2));
    stream
        .set_read_timeout(wait)
        .and_then(|()| stream.set_write_timeout(wait))
        .map_err(fail)?;
    let request =
        serde_json::json!({"id": "taskr-tui", "method": "pane.focus", "params": {"pane_id": pane}});
    stream
        .write_all(format!("{request}\n").as_bytes())
        .map_err(fail)?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).map_err(fail)?;
    let reply: serde_json::Value =
        serde_json::from_str(&line).map_err(|_| "herdr: unreadable reply")?;
    match &reply["error"] {
        serde_json::Value::Null => Ok(()),
        error => Err(format!(
            "herdr: {}",
            error["message"].as_str().unwrap_or("refused")
        )),
    }
}

/// The escape sequence that puts `text` on the clipboard of the terminal the owner sits at
/// (OSC 52). Herdr relays it from a pane to the client.
pub fn osc52(text: &str) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::from("\x1b]52;c;");
    for chunk in text.as_bytes().chunks(3) {
        let n = chunk.iter().fold(0u32, |n, &b| n << 8 | u32::from(b)) << (8 * (3 - chunk.len()));
        for i in 0..4 {
            let sextet = (n >> (18 - 6 * i)) & 63;
            out.push(if i <= chunk.len() {
                ABC[sextet as usize] as char
            } else {
                '='
            });
        }
    }
    out + "\x07"
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::net::UnixListener, path::PathBuf, process, thread};

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("taskr-tui-{name}-{}", process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    #[test]
    fn the_clipboard_sequence_is_base64() {
        assert_eq!(osc52("a"), "\x1b]52;c;YQ==\x07");
        assert_eq!(osc52("ab"), "\x1b]52;c;YWI=\x07");
        assert_eq!(osc52("taskr ✓"), "\x1b]52;c;dGFza3Ig4pyT\x07");
    }

    #[test]
    fn jobs_name_their_taskr_arguments() {
        let answer = |prompt| Job::Answer {
            ask_id: 7,
            text: "A: yes".into(),
            prompt,
        };
        assert_eq!(answer(false).args().unwrap(), ["answer", "7", "A: yes"]);
        assert_eq!(
            answer(true).args().unwrap(),
            ["answer", "7", "A: yes", "--prompt"]
        );
        let park = |park| Job::Park {
            root: 3,
            name: "x".into(),
            park,
        };
        assert_eq!(
            park(true).args().unwrap(),
            ["set", "3", "glance.state=parked"]
        );
        assert_eq!(park(false).args().unwrap(), ["set", "3", "glance.state="]);
    }

    #[test]
    fn focus_asks_the_socket_and_reads_the_refusal() {
        let dir = scratch("sock");
        let path = dir.join("herdr.sock");
        let listener = UnixListener::bind(&path).expect("a socket");
        let server = thread::spawn(move || {
            let mut seen = vec![];
            for reply in [
                r#"{"id":"taskr-tui","result":{}}"#,
                r#"{"id":"taskr-tui","error":{"code":"pane_not_found","message":"pane w9:p9 not found"}}"#,
            ] {
                let (stream, _) = listener.accept().expect("a client");
                let mut line = String::new();
                BufReader::new(&stream)
                    .read_line(&mut line)
                    .expect("a request");
                seen.push(line);
                (&stream)
                    .write_all(format!("{reply}\n").as_bytes())
                    .expect("a reply");
            }
            seen
        });
        let socket = path.to_str().unwrap();
        assert_eq!(focus(socket, "w1:p2"), Ok(()));
        assert_eq!(
            focus(socket, "w9:p9"),
            Err("herdr: pane w9:p9 not found".into())
        );
        let seen = server.join().unwrap();
        assert!(
            seen[0].contains(r#""method":"pane.focus""#)
                && seen[0].contains(r#""pane_id":"w1:p2""#)
        );
        assert!(focus(dir.join("none.sock").to_str().unwrap(), "w1:p2").is_err());
        let _ = fs::remove_dir_all(dir);
    }

    /// The write paths against a scratch ledger, never the live one: `TASKR_DB` and `HOME`
    /// point into a temporary directory and the worker's own task is taken out of the
    /// environment.
    #[test]
    fn answer_and_park_write_to_a_scratch_ledger() {
        let dir = scratch("ledger");
        let db = dir.join("taskr.db");
        let mut taskr: Vec<String> = [
            "env",
            "-u",
            "TASKR_TASK",
            "-u",
            "TASKR_LAUNCH",
            "-u",
            "HERDR_SOCKET_PATH",
        ]
        .map(String::from)
        .into();
        taskr.extend([
            format!("TASKR_DB={}", db.display()),
            format!("HOME={}", dir.display()),
        ]);
        taskr.extend(client::taskr());
        let run = |args: &[&str]| {
            let command: Vec<String> = taskr
                .iter()
                .cloned()
                .chain(args.iter().map(|a| a.to_string()))
                .collect();
            client::output(&command, client::TIMEOUT)
                .map(|out| String::from_utf8_lossy(&out).into_owned())
        };
        if run(&["version"]).is_err() {
            eprintln!("skipped: no taskr binary to run against the scratch ledger");
            return;
        }
        run(&["new", "demo", "--role", "orchestrator"]).expect("a root");
        run(&[
            "ask",
            "Ship it? (A) yes [recommended]; (B) no",
            "--owner",
            "--as",
            "1",
        ])
        .expect("an ask");
        assert!(db.exists(), "the scratch ledger is the one written");
        let open = |run: &dyn Fn(&[&str]) -> Result<String, String>| {
            let glance: crate::model::Glance =
                serde_json::from_str(&run(&["--json", "glance"]).unwrap()).unwrap();
            glance.needs_you.len()
        };
        assert_eq!(open(&run), 1);

        Job::Answer {
            ask_id: 1,
            text: "A: yes".into(),
            prompt: false,
        }
        .run(&taskr)
        .expect("the answer");
        assert_eq!(open(&run), 0);
        // The ledger refuses a second answer, and the view gets its words.
        let again = Job::Answer {
            ask_id: 1,
            text: "B: no".into(),
            prompt: false,
        }
        .run(&taskr);
        assert!(again.is_err(), "{again:?}");

        let park = |park| Job::Park {
            root: 1,
            name: "demo".into(),
            park,
        };
        park(true).run(&taskr).expect("parked");
        assert!(
            run(&["--json", "log", "1"])
                .unwrap()
                .contains("glance.state")
        );
        park(false).run(&taskr).expect("unparked");
        let _ = fs::remove_dir_all(dir);
    }
}
