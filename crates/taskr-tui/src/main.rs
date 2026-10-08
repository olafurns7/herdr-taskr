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
    App, actions, client, draw, frames,
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
    let taskr = client::taskr();
    let pace = client::Pace::default();
    let (updates, wake, events) = if options.demo {
        app.data = frames::fixture();
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
        loop {
            if live.poll(&mut app, Instant::now()) {
                dirty = true;
                // The campaign on screen follows the glance.
                if let Some(job) = input::follow(&app) {
                    start(job);
                }
            }
            for (job, result) in results.try_iter() {
                // What was written shows at once, not at the next tick.
                if !job.reads() {
                    let _ = wake.send(());
                }
                input::done(&mut app, &job, result);
                dirty = true;
            }
            // Draw on a change only: an idle pane costs a poll, not a frame.
            if dirty || app.fetch.face() != face {
                terminal.draw(|f| draw(f, &app))?;
                (face, dirty) = (app.fetch.face(), false);
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
                Effect::Refresh => drop(wake.send(())),
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
