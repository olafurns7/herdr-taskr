//! What a key can do outside the view: focus a Herdr pane, answer an ask, park a campaign,
//! copy text. The view decides and confirms; this module only runs the job.

use std::{
    env,
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    time::Duration,
};

use crate::{
    client,
    model::{Campaign, DocRow, Slotr},
};

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
    /// `taskr campaign ROOT --all`. `open` shows it when it arrives; a refresh does not.
    Campaign { root: i64, name: String, open: bool },
    /// `taskr doc get ID`: the text of a document the campaign read listed.
    Doc { row: DocRow },
    /// `taskr --json slotr`: slotr's pools, read on the hub.
    Slotr,
}

/// What a finished job hands back.
#[derive(Debug, Clone)]
pub enum Done {
    Ok,
    /// Done, with something the owner should know.
    Note(String),
    Campaign(Box<Campaign>),
    Doc(String),
    Slotr(Box<Slotr>),
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
            Job::Campaign { name, .. } => format!("read {name}"),
            Job::Doc { row } => format!("read {} {}", row.kind, row.id),
            Job::Slotr => "read slotr".into(),
        }
    }

    /// A read changes nothing; everything else writes or moves the owner's screen.
    pub fn reads(&self) -> bool {
        matches!(self, Job::Campaign { .. } | Job::Doc { .. } | Job::Slotr)
    }

    /// The taskr arguments, for the jobs taskr runs. Values follow `--`, so a text that
    /// starts with `-` is not taken for a flag.
    pub fn args(&self) -> Option<Vec<String>> {
        let args = |args: &[&str]| Some(args.iter().map(|a| a.to_string()).collect());
        match self {
            Job::Focus { .. } => None,
            Job::Answer {
                ask_id,
                text,
                prompt,
            } => {
                let prompt: &[&str] = if *prompt { &["--prompt"] } else { &[] };
                args(
                    &[
                        &["--json", "answer"],
                        prompt,
                        &["--", &ask_id.to_string(), text],
                    ]
                    .concat(),
                )
            }
            Job::Park { root, park, .. } => {
                let state = if *park {
                    "glance.state=parked"
                } else {
                    "glance.state="
                };
                args(&["--json", "set", "--", &root.to_string(), state])
            }
            Job::Campaign { root, .. } => args(&["--json", "campaign", &root.to_string(), "--all"]),
            Job::Doc { row } => args(&["doc", "get", &row.id.to_string()]),
            Job::Slotr => args(&["--json", "slotr"]),
        }
    }

    /// Runs the job and waits for it. `taskr` is the command that reaches the ledger.
    pub fn run(&self, taskr: &[String]) -> Result<Done, String> {
        let Some(args) = self.args() else {
            let Job::Focus { pane } = self else {
                return Ok(Done::Ok);
            };
            let socket = env::var("HERDR_SOCKET_PATH").map_err(|_| "not in a Herdr pane")?;
            return focus(&socket, pane).map(|()| Done::Ok);
        };
        let command: Vec<String> = taskr.iter().cloned().chain(args).collect();
        let out = client::run(&command, client::TIMEOUT)?;
        match self {
            Job::Answer { prompt, .. } => {
                // An answer can be recorded and still fail to reach the asker's pane: the
                // reply then carries both the answer and the error.
                let reply: serde_json::Value =
                    serde_json::from_slice(&out.stdout).unwrap_or_default();
                if reply["answer_id"].as_i64().is_none() {
                    return Err(out.reason());
                }
                let waiting = reply["asker_waiting"].as_bool().unwrap_or(true);
                Ok(match reply["error"].as_str() {
                    Some(error) => Done::Note(format!("not delivered to its pane: {error}")),
                    None if !waiting && !prompt => {
                        Done::Note("the asker stopped waiting; prompt it by hand".into())
                    }
                    None => Done::Ok,
                })
            }
            Job::Campaign { .. } => {
                let campaign = serde_json::from_slice(&out.text()?)
                    .map_err(|e| format!("unreadable campaign: {e}"))?;
                Ok(Done::Campaign(Box::new(campaign)))
            }
            Job::Slotr => {
                let slotr = serde_json::from_slice(&out.text()?)
                    .map_err(|e| format!("unreadable slotr read: {e}"))?;
                Ok(Done::Slotr(Box::new(slotr)))
            }
            Job::Doc { .. } => Ok(Done::Doc(
                String::from_utf8_lossy(&out.text()?).into_owned(),
            )),
            _ => out.text().map(|_| Done::Ok),
        }
    }
}

/// A reply longer than this is not one of Herdr's.
const REPLY_LIMIT: u64 = 256 * 1024;

/// The socket method `pane.focus`: workspace, tab and pane in one call, for any pane. The
/// herdr CLI has no form for it (`herdr pane focus` is directional), and the socket cannot
/// start a server the way the CLI can.
pub fn focus(socket: &str, pane: &str) -> Result<(), String> {
    const ID: &str = "taskr-tui";
    let fail = |e: std::io::Error| format!("herdr: {e}");
    let mut stream = UnixStream::connect(socket).map_err(fail)?;
    let wait = Some(Duration::from_secs(2));
    stream
        .set_read_timeout(wait)
        .and_then(|()| stream.set_write_timeout(wait))
        .map_err(fail)?;
    let request =
        serde_json::json!({"id": ID, "method": "pane.focus", "params": {"pane_id": pane}});
    stream
        .write_all(format!("{request}\n").as_bytes())
        .map_err(fail)?;
    let mut line = String::new();
    BufReader::new(stream.take(REPLY_LIMIT))
        .read_line(&mut line)
        .map_err(fail)?;
    let reply: serde_json::Value =
        serde_json::from_str(&line).map_err(|_| "herdr: unreadable reply")?;
    if reply["id"] != ID {
        return Err("herdr: a reply to another request".into());
    }
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
            text: "-B: no".into(),
            prompt,
        };
        assert_eq!(
            answer(false).args().unwrap(),
            ["--json", "answer", "--", "7", "-B: no"]
        );
        assert_eq!(
            answer(true).args().unwrap(),
            ["--json", "answer", "--prompt", "--", "7", "-B: no"]
        );
        let park = |park| Job::Park {
            root: 3,
            name: "x".into(),
            park,
        };
        assert_eq!(
            park(true).args().unwrap(),
            ["--json", "set", "--", "3", "glance.state=parked"]
        );
        assert_eq!(
            park(false).args().unwrap(),
            ["--json", "set", "--", "3", "glance.state="]
        );
        let read = Job::Campaign {
            root: 3,
            name: "x".into(),
            open: true,
        };
        assert_eq!(read.args().unwrap(), ["--json", "campaign", "3", "--all"]);
        assert!(read.reads() && !park(true).reads());
    }

    /// taskr's replies, from a stand-in that prints them: no ledger at all.
    #[test]
    fn replies_are_read_for_what_happened() {
        let fake = |script: &str| -> Vec<String> {
            ["sh", "-c", script, "taskr"].map(String::from).into()
        };
        let answer = |prompt| Job::Answer {
            ask_id: 5,
            text: "A: yes".into(),
            prompt,
        };
        let recorded =
            r#"echo '{"answer_id":6,"ask_id":5,"asker_waiting":true,"delivered":true,"ok":true}'"#;
        assert!(matches!(answer(false).run(&fake(recorded)), Ok(Done::Ok)));
        // Recorded, but the prompt did not reach the pane: not a failure, and said so.
        let undelivered = r#"echo 'taskr answer: no pane' >&2; echo '{"answer_id":6,"ask_id":5,"error":"task 1 has no pane or agent name to prompt","kind":"usage","ok":true}'; exit 2"#;
        let Ok(Done::Note(note)) = answer(true).run(&fake(undelivered)) else {
            panic!("a note")
        };
        assert_eq!(
            note,
            "not delivered to its pane: task 1 has no pane or agent name to prompt"
        );
        // The asker stopped waiting after the snapshot, so no prompt went out.
        let late = r#"echo '{"answer_id":6,"ask_id":5,"asker_waiting":false,"delivered":false,"ok":true}'"#;
        assert!(
            matches!(answer(false).run(&fake(late)), Ok(Done::Note(n)) if n.contains("stopped waiting"))
        );
        assert!(matches!(answer(true).run(&fake(late)), Ok(Done::Ok)));
        let refused =
            r#"echo '{"error":"ask 5 is already answered by event 6","kind":"rejected"}'; exit 6"#;
        assert_eq!(
            answer(false).run(&fake(refused)).unwrap_err(),
            "ask 5 is already answered by event 6"
        );

        // The P1b sample, as `taskr --json campaign` prints it.
        let sample: serde_json::Value =
            serde_json::from_str(include_str!("../tests/p1b-sample.json")).unwrap();
        let dir = scratch("reply");
        fs::write(dir.join("campaign.json"), sample["campaign"].to_string()).unwrap();
        let cat = fake(&format!("cat {}", dir.join("campaign.json").display()));
        let read = Job::Campaign {
            root: 1,
            name: "demo-checkout".into(),
            open: true,
        };
        let Ok(Done::Campaign(c)) = read.run(&cat) else {
            panic!("a campaign")
        };
        assert_eq!(
            (c.root.id, c.lanes.len(), c.docs.len(), c.prs[0].number),
            (1, 1, 2, 123)
        );
        let doc = Job::Doc {
            row: c.docs[0].clone(),
        };
        assert!(
            matches!(doc.run(&fake("printf '# Plan\\nraw text\\n'")), Ok(Done::Doc(body)) if body == "# Plan\nraw text\n")
        );
        assert_eq!(
            read.run(&fake("echo nope")).unwrap_err().split(':').next(),
            Some("unreadable campaign")
        );
        let _ = fs::remove_dir_all(dir);
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
                r#"{"id":"someone-else","result":{}}"#,
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
        // A reply to another request is not an answer to this one.
        assert_eq!(
            focus(socket, "w1:p2"),
            Err("herdr: a reply to another request".into())
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
    /// point into a temporary directory. The worker's own `TASKR_TASK` is left in the
    /// environment on purpose: the view must take it out itself.
    #[test]
    fn answer_and_park_write_to_a_scratch_ledger() {
        let dir = scratch("ledger");
        let db = dir.join("taskr.db");
        let mut taskr: Vec<String> = ["env", "-u", "HERDR_SOCKET_PATH"].map(String::from).into();
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
        for text in [
            "Ship it? (A) yes [recommended]; (B) no",
            "And this?",
            "And that?",
        ] {
            run(&["ask", text, "--owner", "--as", "1"]).expect("an ask");
        }
        assert!(db.exists(), "the scratch ledger is the one written");
        let open = |run: &dyn Fn(&[&str]) -> Result<String, String>| {
            let glance: crate::model::Glance =
                serde_json::from_str(&run(&["--json", "glance"]).unwrap()).unwrap();
            glance
                .needs_you
                .iter()
                .map(|a| a.ask_id)
                .collect::<Vec<_>>()
        };
        let asks = open(&run);
        assert_eq!(asks.len(), 3);
        let answer = |ask_id, text: &str, prompt| {
            Job::Answer {
                ask_id,
                text: text.into(),
                prompt,
            }
            .run(&taskr)
        };

        assert!(matches!(
            answer(asks[0], "A: yes", false),
            Ok(Done::Ok | Done::Note(_))
        ));
        // The ledger refuses a second answer, in its own words.
        let again = answer(asks[0], "B: no", false).unwrap_err();
        assert!(
            again.contains("already answered") && !again.contains('{'),
            "{again}"
        );
        // A text that starts with a dash is a text.
        assert!(answer(asks[1], "-B: no, hold it", false).is_ok());
        // The lead has no pane: the answer is recorded and the failed prompt is a note.
        let note = answer(asks[2], "A: yes", true);
        assert!(
            matches!(&note, Ok(Done::Note(n)) if n.starts_with("not delivered")),
            "{note:?}"
        );
        assert!(open(&run).is_empty());

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

        // The reads, where the binary has them (`campaign` is P1b).
        fs::write(dir.join("goal.md"), "# Goal\nShip it.\n").unwrap();
        run(&[
            "doc",
            "set",
            "1",
            "goal",
            "--file",
            dir.join("goal.md").to_str().unwrap(),
        ])
        .expect("a goal");
        match (Job::Campaign {
            root: 1,
            name: "demo".into(),
            open: true,
        })
        .run(&taskr)
        {
            Ok(Done::Campaign(c)) => {
                assert_eq!(
                    (c.root.name.as_str(), c.docs.len(), c.goal[0].as_str()),
                    ("demo", 1, "# Goal")
                );
                let doc = Job::Doc {
                    row: c.docs[0].clone(),
                }
                .run(&taskr);
                assert!(
                    matches!(&doc, Ok(Done::Doc(body)) if body.starts_with("# Goal")),
                    "{doc:?}"
                );
            }
            Err(error) if error.contains("unknown command") => {
                eprintln!("skipped the campaign read: this taskr has none")
            }
            other => panic!("{other:?}"),
        }
        let _ = fs::remove_dir_all(dir);
    }
}
