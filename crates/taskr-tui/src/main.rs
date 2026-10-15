//! `taskr-tui`: the owner's view in a pane. The loop draws, takes keys and the mouse, and
//! carries out what they ask for; fetches and writes run on their own threads, so input
//! never waits on the ledger.

use std::{
    env,
    fs::OpenOptions,
    io::{self, IsTerminal, Write},
    os::unix::fs::OpenOptionsExt,
    panic,
    process::ExitCode,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use ratatui::crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event,
    },
    execute, terminal,
};
use taskr_tui::{
    App, Screen, actions, client, config, frames, github, health,
    input::{self, Effect},
    model::Data,
    theme,
};

const USAGE: &str = "usage: taskr-tui [--theme dark|light|terminal] [--ascii] [--demo]
The owner's view of the taskr ledger. ? lists the keys; q or ctrl-c quits.
  --theme   override the theme (also TASKR_THEME); default: from the terminal's background
  --ascii   draw with ASCII characters only
  --demo    show invented data and write nothing: no ledger is read, no key sends anything
NO_COLOR turns colour off. TASKR_BIN names the taskr binary to read from.";

/// How long the loop sleeps between looks at the fetch thread; also the spinner's clock.
const TICK: Duration = Duration::from_millis(100);
/// Between slotr reads while its view is on top. None run while it is not.
const SLOTR_EVERY: Duration = Duration::from_secs(3);

struct Options {
    theme: Option<String>,
    ascii: bool,
    demo: bool,
}

fn options(args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut out = Options {
        theme: None,
        ascii: false,
        demo: false,
    };
    let mut args = args.skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--ascii" => out.ascii = true,
            "--demo" => out.demo = true,
            "--theme" => {
                let name = args.next().filter(|n| theme::NAMES.contains(&n.as_str()));
                out.theme = Some(name.ok_or("--theme takes dark, light or terminal")?);
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(out)
}

#[cfg(target_os = "linux")]
const O_NONBLOCK: i32 = 0o4000;
#[cfg(not(target_os = "linux"))]
const O_NONBLOCK: i32 = 0x0004;

/// Whether the terminal's background is light, when it says so within 300 ms. The
/// question goes to `/dev/tty` on its own non-blocking handle, which is closed before the
/// view starts: nothing is left behind to take the owner's keys. No `/dev/tty`, no
/// answer: dark.
fn light_background() -> Option<bool> {
    let mut tty = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(O_NONBLOCK)
        .open("/dev/tty")
        .ok()?;
    terminal::enable_raw_mode().ok()?;
    let reply = theme::query(&mut tty, Duration::from_millis(300));
    let _ = terminal::disable_raw_mode();
    theme::light_background(&reply)
}

fn main() -> ExitCode {
    let options = match options(env::args()) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("taskr-tui: {error}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    if !io::stdout().is_terminal() || !io::stdin().is_terminal() {
        eprintln!("taskr-tui: needs a terminal; `taskr glance` prints the same snapshot");
        return ExitCode::from(2);
    }
    let var = |key: &str| env::var(key).ok();
    let named = options.theme.is_some() || var("TASKR_THEME").is_some_and(|v| !v.is_empty());
    let colourless = var("NO_COLOR").is_some_and(|v| !v.is_empty());
    let light = if named || colourless {
        None
    } else {
        light_background()
    };

    let mut app = App::new(Data::default());
    app.theme = theme::choose(options.theme.as_deref(), var, true, light);
    // `t` cycles what this terminal can show; the chosen theme is one of the three.
    let pick = |name| theme::choose(Some(name), var, true, None);
    app.themes = [pick("dark"), pick("light"), pick("terminal")];
    app.ascii = options.ascii;
    // Demo never resolves, reads or writes a user config path.
    let config_path = if options.demo { None } else { config::path() };
    if let Some(path) = &config_path {
        (app.config, app.config_warning) = config::Config::load(path);
    } else if !options.demo {
        app.config_warning = Some("TUI config path unavailable; choices not saved".into());
    }
    let config_ok = app.config_warning.is_none();
    if options.demo {
        app.config.repos = vec!["example-org/app".into()];
        app.config.issues_repo = Some("example-org/app".into());
        app.config.health_repo = Some("example-org/app".into());
        app.config.panels[3..6].fill(true);
    }
    // The login is read by the poller, in memory only; demo shows an invented one.
    app.mine = if options.demo {
        taskr_tui::prs::demo(app.observed_at)
    } else {
        taskr_tui::prs::Mine::new(&app.config.repos)
    };
    app.shipped = if options.demo {
        github::demo_shipped(app.observed_at)
    } else {
        github::Dashboard::for_kind(&app.config.repos, github::Kind::Shipped)
    };
    app.builds = if options.demo {
        github::demo_builds(app.observed_at)
    } else {
        github::Dashboard::for_kind(&app.config.repos, github::Kind::Build)
    };
    app.issues = if options.demo {
        github::demo_issues(app.observed_at)
    } else {
        github::Dashboard::for_kind(
            &app.config.issues_repo.iter().cloned().collect::<Vec<_>>(),
            github::Kind::Issues,
        )
    };
    // Demo shows an invented note and never reads.
    if options.demo {
        app.health = health::demo(app.observed_at);
    } else {
        // Hidden, and never read, unless both health_repo and health_root are set.
        if let Some((repo, root)) = app.config.health() {
            (app.health.repo, app.health.root) = (Some(repo), root);
        }
    }
    let mut hub_health = health::Poller::new(health::command);
    let mut issues = github::Poller::new(github::Kind::Issues);
    let mut shipped = github::Poller::new(github::Kind::Shipped);
    let mut builds = github::Poller::new(github::Kind::Build);
    let mut mine = taskr_tui::prs::Poller::new(taskr_tui::prs::gh);
    let taskr = client::taskr();
    let pace = client::Pace::default();
    let (updates, wake, events) = if options.demo {
        app.data = frames::fixture();
        frames::with_structured(&mut app);
        app.fetch.loaded = true;
        // No fetch thread: the receiver stays empty and wakes go nowhere.
        (mpsc::channel().1, mpsc::channel().0, None)
    } else {
        let (updates, wake, events) =
            client::spawn(client::command(), Some(client::events_command()), pace);
        (updates, wake, Some(events))
    };
    let mut live = client::Live::new(updates, pace, events.clone());
    let (finished, results) = mpsc::channel();
    let start = |job: actions::Job| {
        let (taskr, finished) = (taskr.clone(), finished.clone());
        thread::spawn(move || {
            let result = job.run(&taskr);
            let _ = finished.send((job, result));
        });
    };

    let mut terminal = ratatui::init();
    // `init` restores the screen if the view panics; the modes set here are added to that.
    let hook = panic::take_hook();
    let subscription = events.clone();
    panic::set_hook(Box::new(move |info| {
        // A worker's panic does not end the view, so it must not end the stream or restore
        // the screen under it. ponytail: its message is dropped; log it if one ever fires.
        if thread::current().name() != Some("main") {
            return;
        }
        // The subscription's child must not outlive the view.
        if let Some(events) = &subscription {
            events.stop();
        }
        let _ = execute!(io::stdout(), DisableMouseCapture, DisableBracketedPaste);
        hook(info);
    }));
    let _ = execute!(io::stdout(), EnableMouseCapture, EnableBracketedPaste);
    let result = (|| -> io::Result<()> {
        // What the last frame showed of the fetch: the spinner, the age, the countdown.
        let mut face = String::new();
        let mut dirty = true;
        let mut shown = None;
        let mut pr_manual = false;
        // The slotr view's read: due at once when it opens (None), then every 3 s.
        let (mut slotr_due, mut slotr_started, mut slotr_at) =
            (None::<Instant>, None::<Instant>, None::<Instant>);
        loop {
            if live.poll(&mut app, Instant::now()) {
                dirty = true;
                // The campaign on screen follows the glance.
                if let Some(job) = input::follow(&app) {
                    start(job);
                }
            }
            for (job, result) in results.try_iter() {
                // Taken first: an unknown answer's time is before the read the wake starts.
                input::done(&mut app, &job, result);
                // What was written shows at once, not at the next tick.
                if !job.reads() {
                    let _ = wake.send(());
                }
                if job == actions::Job::Slotr {
                    let now = Instant::now();
                    (slotr_started, slotr_due) = (None, Some(now + SLOTR_EVERY));
                    if app.slotr.error.is_none() {
                        slotr_at = Some(now);
                    }
                }
                dirty = true;
            }
            let now = Instant::now();
            if !options.demo {
                dirty |= app.advance_time(time::OffsetDateTime::now_utc().unix_timestamp(), now);
                // A source reads only while its card is on the Dashboard with nothing over it.
                // Navigation never starts one: a screen switch keeps each source's schedule.
                let ship_visible = app.admits(4);
                let build_visible = app.admits(5);
                dirty |= shipped.tick(&mut app, now, ship_visible, pr_manual);
                dirty |= builds.tick(&mut app, now, build_visible, pr_manual);
                // Read afresh every tick: My PRs' stage 2 needs the admission of the tick
                // its stage 1 lands on.
                let visible = app.admits(3);
                let wall = time::OffsetDateTime::now_utc().unix_timestamp();
                dirty |= mine.tick(&mut app, now, wall, visible, pr_manual);
                // Manual `r` reaches it, but its admission budget ignores manual.
                let issues_visible = app.admits(9);
                dirty |= issues.tick(&mut app, now, issues_visible, pr_manual);
                // Its own minute budget; manual `r` does not reach it.
                let health_visible = app.admits(10);
                dirty |= hub_health.tick(&mut app, now, health_visible);
            }
            pr_manual = false;
            if app.screen != Screen::Slotr {
                slotr_due = None;
            } else if slotr_started.is_none() && slotr_due.is_none_or(|d| now >= d) {
                if options.demo {
                    input::demo(&mut app, actions::Job::Slotr);
                    (slotr_due, slotr_at) = (Some(now + SLOTR_EVERY), Some(now));
                } else {
                    slotr_started = Some(now);
                    start(actions::Job::Slotr);
                }
                dirty = true;
            }
            app.slotr.age_ms = slotr_at.map_or(0, |at| now.duration_since(at).as_millis() as i64);
            (app.slotr.in_flight, app.slotr.tick) = client::spinner(slotr_started, now);
            // Draw on a change only: an idle pane costs a poll, not a frame.
            let faces = app.fetch.face()
                + &app.slotr.face()
                + &app.mine.status(now)
                + &[&app.shipped, &app.builds, &app.issues]
                    .into_iter()
                    .flat_map(|d| &d.sources)
                    .map(|s| s.status(now))
                    .collect::<String>()
                + &app.health.face(app.observed_at);
            if dirty || faces != face {
                taskr_tui::frame(&mut terminal, &app, &mut shown)?;
                (face, dirty) = (faces, false);
            }
            if !event::poll(TICK)? {
                continue;
            }
            dirty = true;
            let effect = match event::read()? {
                Event::Key(key) => input::key(&mut app, key),
                Event::Mouse(mouse) => input::mouse(&mut app, mouse, Instant::now()),
                Event::Paste(text) => {
                    input::paste(&mut app, &text);
                    Effect::None
                }
                _ => Effect::None,
            };
            match effect {
                Effect::None => {}
                Effect::Quit => return Ok(()),
                Effect::Refresh => {
                    let _ = wake.send(());
                    slotr_due = None;
                    pr_manual = true;
                }
                Effect::SaveConfig => {
                    if config_ok && let Some(path) = &config_path {
                        app.config_warning =
                            app.config.save(path).err().map(|_| {
                                "TUI config not saved; choices kept for this session".into()
                            });
                    }
                }
                Effect::Redraw => shown = None,
                Effect::Copy(text) => {
                    let mut out = io::stdout();
                    out.write_all(actions::osc52(&text).as_bytes())?;
                    out.flush()?;
                }
                Effect::Mouse(true) => execute!(io::stdout(), EnableMouseCapture)?,
                Effect::Mouse(false) => execute!(io::stdout(), DisableMouseCapture)?,
                Effect::Run(job) if options.demo => input::demo(&mut app, job),
                Effect::Run(job) => start(job),
            }
        }
    })();
    drop(mine);
    drop(shipped);
    drop(builds);
    drop(issues);
    drop(hub_health);
    if let Some(events) = &events {
        events.stop();
    }
    let _ = execute!(io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("taskr-tui: {error}");
            ExitCode::FAILURE
        }
    }
}
