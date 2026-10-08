//! `taskr-tui`: the owner's view in a pane. The loop draws, takes keys and the mouse, and
//! carries out what they ask for; fetches and writes run on their own threads, so input
//! never waits on the ledger.

use std::{
    env,
    io::{self, IsTerminal, Read, Write},
    process::ExitCode,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use ratatui::crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event},
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

const EVERY: Duration = Duration::from_secs(5);
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

/// Asks the terminal for its background colour (OSC 11), then for its device attributes,
/// which every terminal answers: so the read ends even when the first question is ignored.
// ponytail: if a terminal answers neither within 300 ms, the reader thread is left behind
// and swallows the next key. Whether Herdr's pane answers is a P2 check; `--theme` skips this.
fn light_background() -> Option<bool> {
    terminal::enable_raw_mode().ok()?;
    let (tx, rx) = mpsc::channel();
    let asked = io::stdout()
        .write_all(b"\x1b]11;?\x07\x1b[c")
        .and_then(|()| io::stdout().flush());
    if asked.is_ok() {
        thread::spawn(move || {
            let mut reply = vec![];
            let mut byte = [0u8; 1];
            while io::stdin().read(&mut byte).is_ok_and(|n| n == 1) {
                reply.push(byte[0]);
                if byte[0] == b'c' && reply.windows(3).any(|w| w == b"\x1b[?") {
                    break;
                }
            }
            let _ = tx.send(reply);
        });
    }
    let reply = rx.recv_timeout(Duration::from_millis(300)).ok();
    let _ = terminal::disable_raw_mode();
    theme::light_background(&reply?)
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
    let (updates, wake) = if options.demo {
        app.data = frames::fixture();
        app.fetch.loaded = true;
        // No fetch thread: the receiver stays empty and wakes go nowhere.
        (mpsc::channel().1, mpsc::channel().0)
    } else {
        client::spawn(client::command(), EVERY)
    };
    let mut live = client::Live::new(updates, EVERY);
    let (finished, results) = mpsc::channel();

    // `init` also restores the terminal if the view panics.
    let mut terminal = ratatui::init();
    let _ = execute!(io::stdout(), EnableMouseCapture);
    let result = (|| -> io::Result<()> {
        loop {
            live.poll(&mut app, Instant::now());
            for (job, result) in results.try_iter() {
                input::done(&mut app, &job, result);
                // What was written shows at once, not at the next tick.
                let _ = wake.send(());
            }
            // ratatui writes only the cells that changed, so an idle tick writes nothing.
            terminal.draw(|f| draw(f, &app))?;
            if !event::poll(TICK)? {
                continue;
            }
            let effect = match event::read()? {
                Event::Key(key) => input::key(&mut app, key),
                Event::Mouse(mouse) => input::mouse(&mut app, mouse, Instant::now()),
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
                Effect::Run(job) if options.demo => {
                    app.status = Some(format!("demo: {} (nothing was sent)", job.label()));
                }
                Effect::Run(job) => {
                    let (taskr, finished) = (taskr.clone(), finished.clone());
                    thread::spawn(move || {
                        let result = job.run(&taskr);
                        let _ = finished.send((job, result));
                    });
                }
            }
        }
    })();
    let _ = execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("taskr-tui: {error}");
            ExitCode::FAILURE
        }
    }
}
