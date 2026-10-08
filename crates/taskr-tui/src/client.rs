//! The live client: `taskr --json glance` in a background thread every few seconds, so
//! drawing and input never wait on the ledger. The campaign read (`taskr campaign ID`)
//! joins it when P1b ships.

use std::{
    env,
    io::Read,
    process::{Command, ExitStatus, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender},
    thread,
    time::{Duration, Instant},
};

use crate::{App, model::Glance};

/// A fetch that takes longer than this shows the spinner.
const SPINNER_AFTER: Duration = Duration::from_millis(300);
const SPINNER_FRAME: Duration = Duration::from_millis(150);
/// A taskr that has not answered by now is killed, and the frame goes stale.
pub const TIMEOUT: Duration = Duration::from_secs(10);

pub enum Update {
    Started,
    Done(Box<Result<Glance, String>>),
}

/// The command that reaches the ledger: `taskr`, or the binary `TASKR_BIN` names.
pub fn taskr() -> Vec<String> {
    let bin = env::var("TASKR_BIN").ok().filter(|b| !b.is_empty());
    vec![bin.unwrap_or_else(|| "taskr".into())]
}

/// The snapshot command: `taskr --json glance`.
pub fn command() -> Vec<String> {
    let mut command = taskr();
    command.extend(["--json".into(), "glance".into()]);
    command
}

/// What a finished command left behind.
pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Output {
    /// Why the command failed, in one line short enough for the footer. taskr says it as
    /// JSON (`{"error":…}` with `--json`, `x1 N {"err":…}` without); anything else is
    /// taken as its first line.
    pub fn reason(&self) -> String {
        let (stdout, stderr) = (
            String::from_utf8_lossy(&self.stdout),
            String::from_utf8_lossy(&self.stderr),
        );
        let lines = || {
            stderr
                .lines()
                .chain(stdout.lines())
                .map(str::trim)
                .filter(|l| !l.is_empty())
        };
        let said = stdout.lines().chain(stderr.lines()).find_map(|line| {
            let reply: serde_json::Value = serde_json::from_str(&line[line.find('{')?..]).ok()?;
            let error = reply.get("error").or(reply.get("err"))?;
            Some(error.as_str()?.to_string())
        });
        let line = said.or_else(|| lines().next().map(String::from));
        line.map_or_else(|| self.status.to_string(), |l| l.chars().take(80).collect())
    }

    /// What it printed, or why it failed.
    pub fn text(self) -> Result<Vec<u8>, String> {
        if self.status.success() {
            Ok(self.stdout)
        } else {
            Err(self.reason())
        }
    }
}

/// Runs the command once and decodes its snapshot.
pub fn fetch(command: &[String], timeout: Duration) -> Result<Glance, String> {
    let stdout = output(command, timeout)?;
    serde_json::from_slice(&stdout).map_err(|e| format!("unreadable snapshot: {e}"))
}

/// Runs a command to its end and returns what it printed, or one line on why it failed.
pub fn output(command: &[String], timeout: Duration) -> Result<Vec<u8>, String> {
    run(command, timeout)?.text()
}

/// Runs a command to its end. An error here means it never ran or never finished.
pub fn run(command: &[String], timeout: Duration) -> Result<Output, String> {
    let (bin, args) = command.split_first().ok_or("no command")?;
    let mut child = Command::new(bin)
        .args(args)
        // The view never reads or writes as a lead, whatever pane it was started in.
        .env_remove("TASKR_TASK")
        .env_remove("TASKR_LAUNCH")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run {bin}: {e}"))?;
    // Both pipes are read off this thread, so a large snapshot cannot fill one and stall
    // the child, and a child that hangs can still be killed.
    let read = |mut pipe: Box<dyn Read + Send>| {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut bytes = vec![];
            let _ = pipe.read_to_end(&mut bytes);
            let _ = tx.send(bytes);
        });
        rx
    };
    let out = read(Box::new(child.stdout.take().expect("a piped stdout")));
    let err = read(Box::new(child.stderr.take().expect("a piped stderr")));
    let stdout = match out.recv_timeout(timeout) {
        Ok(bytes) => bytes,
        Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{bin} did not answer in {}s", timeout.as_secs()));
        }
    };
    let status = child.wait().map_err(|e| format!("{bin}: {e}"))?;
    let stderr = err.recv_timeout(Duration::from_secs(1)).unwrap_or_default();
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Fetches now and then every `every`, or sooner when `wake` is sent to (`r`, or after a
/// write), until the receiver is dropped.
pub fn spawn(command: Vec<String>, every: Duration) -> (Receiver<Update>, Sender<()>) {
    let (tx, rx) = mpsc::channel();
    let (wake, woken) = mpsc::channel();
    thread::spawn(move || {
        while tx.send(Update::Started).is_ok()
            && tx
                .send(Update::Done(Box::new(fetch(&command, TIMEOUT))))
                .is_ok()
        {
            let _ = woken.recv_timeout(every);
            // Several wakes are one refresh.
            woken.try_iter().for_each(drop);
        }
    });
    (rx, wake)
}

/// Turns the fetch thread's updates into the view's state: the snapshot, its age, the
/// spinner, and the stale and no-data states.
pub struct Live {
    updates: Receiver<Update>,
    every: Duration,
    started: Option<Instant>,
    fetched: Option<Instant>,
    next: Option<Instant>,
}

impl Live {
    pub fn new(updates: Receiver<Update>, every: Duration) -> Self {
        Self {
            updates,
            every,
            started: None,
            fetched: None,
            next: None,
        }
    }

    /// Applies what has arrived, without waiting, and says whether a new snapshot did.
    pub fn poll(&mut self, app: &mut App, now: Instant) -> bool {
        let mut fresh = false;
        for update in self.updates.try_iter() {
            match update {
                Update::Started => self.started = Some(now),
                Update::Done(result) => {
                    self.started = None;
                    self.next = Some(now + self.every);
                    match *result {
                        Ok(glance) => {
                            // The last good snapshot stays on screen through a failure.
                            app.snapshot(glance);
                            app.fetch.loaded = true;
                            app.fetch.error = None;
                            app.fetch.tries = 0;
                            fresh = true;
                            self.fetched = Some(now);
                        }
                        Err(error) => {
                            app.fetch.error = Some(error);
                            app.fetch.tries += 1;
                        }
                    }
                }
            }
        }
        let running = self
            .started
            .map(|s| now.saturating_duration_since(s))
            .filter(|d| *d > SPINNER_AFTER);
        app.fetch.in_flight = running.is_some();
        app.fetch.tick =
            running.map_or(0, |d| (d.as_millis() / SPINNER_FRAME.as_millis()) as usize);
        app.fetch.age_ms = self
            .fetched
            .map_or(0, |f| now.saturating_duration_since(f).as_millis() as i64);
        app.fetch.retry_in_s = self.next.map_or(0, |n| {
            n.saturating_duration_since(now).as_secs_f32().ceil() as u32
        });
        fresh
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Data;

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    #[test]
    fn a_fetch_decodes_the_snapshot_or_says_why_not() {
        let json = r#"{"verdict":"needs_you","needs_you":[{"ask_id":7,"blocking":true,"future_field":1}],"quiet":{"count":2}}"#;
        let glance = fetch(&sh(&format!("echo '{json}'")), TIMEOUT).expect("a snapshot");
        assert_eq!((glance.needs_you[0].ask_id, glance.quiet.count), (7, 2));

        let error =
            |script: &str| fetch(&sh(script), Duration::from_millis(300)).expect_err(script);
        assert_eq!(
            error("echo; echo ' server unreachable' >&2; exit 5"),
            "server unreachable"
        );
        assert_eq!(error("exit 3"), "exit status: 3");
        // taskr's own words, with and without --json, not the raw line.
        assert_eq!(
            error(
                r#"echo 'taskr: nope' >&2; echo '{"error":"server unreachable (connection refused)","kind":"transport"}'; exit 5"#
            ),
            "server unreachable (connection refused)"
        );
        assert_eq!(
            error(r#"echo 'x1 6 {"err":"event 99 is not an ask","k":"rejected"}' >&2; exit 6"#),
            "event 99 is not an ask"
        );
        // The view never acts as the task whose pane it was started in.
        let seen = output(
            &sh(r#"echo "${TASKR_TASK-unset} ${TASKR_LAUNCH-unset}""#),
            TIMEOUT,
        )
        .unwrap();
        assert_eq!(String::from_utf8_lossy(&seen).trim(), "unset unset");
        assert!(error("echo not json").starts_with("unreadable snapshot"));
        assert_eq!(error("sleep 5"), "sh did not answer in 0s");
        assert!(
            fetch(&["/nonexistent/taskr".into()], TIMEOUT)
                .expect_err("no binary")
                .starts_with("cannot run")
        );
    }

    #[test]
    fn the_view_goes_from_loading_to_fresh_to_stale() {
        let (tx, rx) = mpsc::channel();
        let mut live = Live::new(rx, Duration::from_secs(5));
        let mut app = App::new(Data::default());
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);

        // Loading: no snapshot, no error; the spinner only after 300 ms.
        tx.send(Update::Started).unwrap();
        live.poll(&mut app, at(0));
        assert!(!app.fetch.loaded && !app.fetch.in_flight);
        live.poll(&mut app, at(450));
        assert!(app.fetch.in_flight && app.fetch.tick == 3);

        // No data: the first fetch failed.
        tx.send(Update::Done(Box::new(Err("server unreachable".into()))))
            .unwrap();
        live.poll(&mut app, at(500));
        assert!(!app.fetch.loaded && app.fetch.error.is_some() && !app.fetch.in_flight);
        assert_eq!((app.fetch.tries, app.fetch.retry_in_s), (1, 5));

        // Fresh.
        let mut glance = Glance::default();
        glance.quiet.count = 4;
        tx.send(Update::Done(Box::new(Ok(glance)))).unwrap();
        live.poll(&mut app, at(1000));
        assert!(app.fetch.loaded && app.fetch.error.is_none() && app.fetch.tries == 0);
        live.poll(&mut app, at(3000));
        assert_eq!((app.fetch.age_ms, app.fetch.retry_in_s), (2000, 3));

        // Stale: the next fetch fails, and the last snapshot stays.
        tx.send(Update::Done(Box::new(Err("timeout".into()))))
            .unwrap();
        live.poll(&mut app, at(6000));
        assert!(app.fetch.loaded && app.fetch.error.as_deref() == Some("timeout"));
        assert_eq!((app.data.glance.quiet.count, app.fetch.age_ms), (4, 5000));
    }
}
