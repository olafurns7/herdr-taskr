//! The live client: `taskr --json glance` in a background thread every few seconds, so
//! drawing and input never wait on the ledger. The campaign read (`taskr campaign ID`)
//! joins it when P1b ships.

use std::{
    env,
    io::{BufRead, BufReader, Read},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
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

/// The command that subscribes to the hub's change notifications: `taskr _events --json`,
/// one JSON line per notification. Only a Rust hub has it.
pub fn events_command() -> Vec<String> {
    let mut command = taskr();
    command.extend(["_events".into(), "--json".into()]);
    command
}

/// How often things happen. The defaults are the product's; tests run faster.
#[derive(Debug, Clone, Copy)]
pub struct Pace {
    /// Between fetches while there is no subscription.
    pub poll: Duration,
    /// The safety poll while subscribed, in case a notification is lost.
    pub slow: Duration,
    /// A burst of notifications is one fetch: never two fetches closer than this.
    pub gap: Duration,
    /// Before the first try to subscribe again; doubles up to `retry_max`.
    pub retry: Duration,
    pub retry_max: Duration,
}

impl Default for Pace {
    fn default() -> Self {
        Self {
            poll: Duration::from_secs(5),
            slow: Duration::from_secs(60),
            gap: Duration::from_millis(250),
            retry: Duration::from_secs(30),
            retry_max: Duration::from_secs(300),
        }
    }
}

/// The subscription: whether it is live, and the child to kill when the view leaves.
#[derive(Debug, Clone, Default)]
pub struct Events {
    live: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
}

impl Events {
    /// Notifications are arriving: the view is pushed to, not polling.
    pub fn live(&self) -> bool {
        self.live.load(Ordering::Relaxed)
    }

    /// Ends the subscription for good and kills its child. Safe to call twice, and from
    /// the panic hook.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
        self.live.store(false, Ordering::Relaxed);
        // A poisoned lock still holds the child.
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mut child) = child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Sleeps, unless the view is leaving.
    fn pause(&self, time: Duration) -> bool {
        let end = Instant::now() + time;
        while Instant::now() < end && !self.stopped.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(50).min(time));
        }
        !self.stopped.load(Ordering::Relaxed)
    }

    /// Runs the subscription until `stop`: one child at a time, a wake per notification,
    /// and a longer wait after each child that ends. A taskr without the command (the Go
    /// one answers "unknown command" and exits 2) is just a child that ends at once.
    fn run(&self, command: &[String], pace: Pace, wake: &Sender<()>) {
        let mut retry = pace.retry;
        while !self.stopped.load(Ordering::Relaxed) {
            let Some((bin, args)) = command.split_first() else {
                return;
            };
            let spawned = Command::new(bin)
                .args(args)
                .env_remove("TASKR_TASK")
                .env_remove("TASKR_LAUNCH")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();
            if let Ok(mut child) = spawned {
                let lines = child.stdout.take().map(BufReader::new);
                *self.child.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
                for line in lines
                    .into_iter()
                    .flat_map(BufRead::lines)
                    .map_while(Result::ok)
                {
                    // A notification names its event (`change`, or `reset`: re-read
                    // everything, which every fetch does). An error line does not.
                    let notice: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
                    if notice["event"].is_string() {
                        self.live.store(true, Ordering::Relaxed);
                        retry = pace.retry;
                        if wake.send(()).is_err() {
                            return self.stop();
                        }
                    }
                }
                // The stream ended: reap the child, unless `stop` already has.
                let child = self.child.lock().unwrap_or_else(|e| e.into_inner()).take();
                if let Some(mut child) = child {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
            // Back to polling, at once, and try again later.
            self.live.store(false, Ordering::Relaxed);
            if wake.send(()).is_err() || !self.pause(retry) {
                return;
            }
            retry = (retry * 2).min(pace.retry_max);
        }
    }
}

/// Fetches now, then on every notification from `events` (bursts coalesced), with a slow
/// safety poll; without a subscription, every `pace.poll`. `wake` asks for a fetch now
/// (`r`, or after a write). Ends when the receiver is dropped.
pub fn spawn(
    command: Vec<String>,
    events: Option<Vec<String>>,
    pace: Pace,
) -> (Receiver<Update>, Sender<()>, Events) {
    let (tx, rx) = mpsc::channel();
    let (wake, woken) = mpsc::channel();
    let subscription = Events::default();
    if let Some(events) = events {
        let (subscription, wake) = (subscription.clone(), wake.clone());
        thread::spawn(move || subscription.run(&events, pace, &wake));
    }
    let live = subscription.clone();
    thread::spawn(move || {
        loop {
            let started = Instant::now();
            if tx.send(Update::Started).is_err()
                || tx
                    .send(Update::Done(Box::new(fetch(&command, TIMEOUT))))
                    .is_err()
            {
                return live.stop();
            }
            let _ = woken.recv_timeout(if live.live() { pace.slow } else { pace.poll });
            // Never two fetches closer than the gap, and several wakes are one fetch.
            thread::sleep(pace.gap.saturating_sub(started.elapsed()));
            woken.try_iter().for_each(drop);
        }
    });
    (rx, wake, subscription)
}

/// Turns the fetch thread's updates into the view's state: the snapshot, its age, the
/// spinner, and the stale and no-data states.
pub struct Live {
    updates: Receiver<Update>,
    pace: Pace,
    /// None when nothing is fetched at all (`--demo`).
    events: Option<Events>,
    started: Option<Instant>,
    fetched: Option<Instant>,
    next: Option<Instant>,
}

impl Live {
    pub fn new(updates: Receiver<Update>, pace: Pace, events: Option<Events>) -> Self {
        Self {
            updates,
            pace,
            events,
            started: None,
            fetched: None,
            next: None,
        }
    }

    /// Applies what has arrived, without waiting, and says whether a new snapshot did.
    pub fn poll(&mut self, app: &mut App, now: Instant) -> bool {
        let mut fresh = false;
        app.fetch.live = self.events.as_ref().map(Events::live);
        let every = if app.fetch.live == Some(true) {
            self.pace.slow
        } else {
            self.pace.poll
        };
        for update in self.updates.try_iter() {
            match update {
                Update::Started => self.started = Some(now),
                Update::Done(result) => {
                    self.started = None;
                    self.next = Some(now + every);
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
        let mut live = Live::new(rx, Pace::default(), None);
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

    /// A stand-in for taskr: `glance` counts its runs in a file, `_events` runs `events`.
    fn fake(name: &str, events: &str) -> (std::path::PathBuf, Vec<String>, Vec<String>) {
        let dir = env::temp_dir().join(format!("taskr-tui-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.display();
        let glance = sh(&format!("echo x >> {d}/fetches; echo '{{}}'"));
        let events = sh(&format!("echo $$ >> {d}/pids; {events}"));
        (dir, glance, events)
    }

    fn count(dir: &std::path::Path, file: &str) -> usize {
        std::fs::read_to_string(dir.join(file)).map_or(0, |s| s.lines().count())
    }

    fn until(what: &str, done: impl Fn() -> bool) {
        let end = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < end, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn alive(dir: &std::path::Path) -> bool {
        let pids = std::fs::read_to_string(dir.join("pids")).unwrap_or_default();
        let pid = pids.lines().last().unwrap_or("0").to_string();
        Command::new("kill")
            .args(["-0", &pid])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    #[test]
    fn notifications_fetch_and_a_burst_is_one_fetch() {
        // Polls are an hour apart here: every fetch after the first is a notification's.
        let hour = Duration::from_secs(3600);
        let pace = Pace {
            poll: hour,
            slow: hour,
            gap: Duration::from_millis(250),
            ..Pace::default()
        };
        let notice = r#"{"event":"change","epoch":"e","rev":7,"seq":1,"kinds":["note"]}"#;
        let reset = r#"{"event":"reset","epoch":"e","rev":9,"seq":4,"kinds":[]}"#;
        let script = format!(
            "echo '{notice}'; echo '{notice}'; echo '{reset}'; sleep 0.6; echo '{notice}'; exec sleep 30"
        );
        let (dir, glance, events) = fake("events", &script);
        let (rx, _wake, subscription) = spawn(glance, Some(events), pace);
        until("the subscription", || subscription.live());
        // The first fetch, one for the burst of three, one for the late notification.
        until("three fetches", || count(&dir, "fetches") >= 3);
        thread::sleep(Duration::from_millis(600));
        assert_eq!(count(&dir, "fetches"), 3);
        // The view shows the mode, and waits the slow interval for its countdown.
        let mut live = Live::new(rx, pace, Some(subscription.clone()));
        let mut app = App::new(Data::default());
        live.poll(&mut app, Instant::now());
        assert_eq!((app.fetch.live, app.fetch.loaded), (Some(true), true));
        // Leaving kills the child, and it is not started again.
        assert!(alive(&dir));
        subscription.stop();
        until("the child to go", || !alive(&dir));
        thread::sleep(Duration::from_millis(200));
        assert_eq!((count(&dir, "pids"), subscription.live()), (1, false));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn without_the_command_the_view_polls_and_tries_again_later() {
        let pace = Pace {
            poll: Duration::from_millis(100),
            slow: Duration::from_secs(3600),
            gap: Duration::from_millis(10),
            retry: Duration::from_millis(300),
            retry_max: Duration::from_millis(400),
        };
        // What the Go taskr says, on stdout with --json, exit 2.
        let unknown = r#"echo '{"error":"unknown command _events","kind":"usage","try":"taskr help"}'; exit 2"#;
        let (dir, glance, events) = fake("unknown", unknown);
        let (rx, _wake, subscription) = spawn(glance, Some(events), pace);
        // It keeps polling at the short interval, never calls itself live, and asks again
        // after the backoff.
        until("polling", || count(&dir, "fetches") >= 4);
        until("a second and a third try", || count(&dir, "pids") >= 3);
        assert!(!subscription.live());
        let mut live = Live::new(rx, pace, Some(subscription.clone()));
        let mut app = App::new(Data::default());
        live.poll(&mut app, Instant::now());
        assert_eq!(app.fetch.live, Some(false));
        subscription.stop();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_line_the_hub_sends_on_connect_makes_the_view_live() {
        let hour = Duration::from_secs(3600);
        let pace = Pace {
            poll: hour,
            slow: hour,
            gap: Duration::from_millis(10),
            ..Pace::default()
        };
        // A quiet ledger: the hub's first event, then nothing.
        let script =
            r#"echo '{"event":"change","epoch":"e","rev":3,"seq":0,"kinds":[]}'; exec sleep 30"#;
        let (dir, glance, events) = fake("connect", script);
        let (rx, _wake, subscription) = spawn(glance, Some(events), pace);
        until("the subscription", || subscription.live());
        let mut live = Live::new(rx, pace, Some(subscription.clone()));
        let mut app = App::new(Data::default());
        live.poll(&mut app, Instant::now());
        assert_eq!(app.fetch.live, Some(true));
        // And it stays live while the stream is open and silent.
        thread::sleep(Duration::from_millis(300));
        assert!(subscription.live());
        subscription.stop();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_stream_that_ends_falls_back_to_polling() {
        let pace = Pace {
            poll: Duration::from_millis(100),
            slow: Duration::from_secs(3600),
            gap: Duration::from_millis(10),
            retry: Duration::from_secs(3600),
            ..Pace::default()
        };
        let script =
            r#"echo '{"event":"change","epoch":"e","rev":1,"seq":1,"kinds":[]}'; sleep 0.5"#;
        let (dir, glance, events) = fake("ends", script);
        let (_rx, _wake, subscription) = spawn(glance, Some(events), pace);
        until("the subscription", || subscription.live());
        let before = count(&dir, "fetches");
        // The hub goes away: no hour-long wait for the slow poll, the short one is back.
        until("the fallback", || !subscription.live());
        until("polling again", || count(&dir, "fetches") >= before + 3);
        subscription.stop();
        let _ = std::fs::remove_dir_all(dir);
    }
}
