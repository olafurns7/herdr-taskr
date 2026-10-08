//! Keys and the mouse (plan §3, Keymap and Mouse). A key changes the view's state and may
//! return an [`Effect`] for the loop to carry out; nothing here blocks or touches the
//! ledger. Provisional until the owner's yes on the frames.

use std::time::{Duration, Instant};

use ratatui::{
    crossterm::event::{
        KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    layout::Position,
};

use crate::{
    App, Hit, Screen,
    actions::Job,
    ask, campaign,
    glance::{self, Selected},
    lists, ui,
};

/// What the loop does after a key.
#[derive(Debug, PartialEq)]
pub enum Effect {
    None,
    Quit,
    /// Run in the background; [`done`] takes the result.
    Run(Job),
    /// Put this on the clipboard (OSC 52). The footer already shows it.
    Copy(String),
    /// Turn mouse reporting on or off.
    Mouse(bool),
    /// Fetch now.
    Refresh,
}

const DOUBLE_CLICK: Duration = Duration::from_millis(400);

pub fn key(app: &mut App, key: KeyEvent) -> Effect {
    if key.kind == KeyEventKind::Release {
        return Effect::None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if ctrl && key.code == KeyCode::Char('c') {
        return Effect::Quit;
    }
    // Any key shows the owner has looked.
    app.status = None;
    app.changed.clear();
    if let Some(job) = app.pending.take() {
        // A write runs on `y` and on nothing else.
        if key.code == KeyCode::Char('y') {
            return Effect::Run(job);
        }
        return say(app, "cancelled, nothing was written");
    }
    if app.typing {
        return filter(app, key.code);
    }
    match app.screen {
        Screen::Answer => answer(app, key.code),
        Screen::Confirm => confirm(app, key.code),
        _ => view(app, key.code, ctrl),
    }
}

fn press(app: &mut App, code: KeyCode) -> Effect {
    key(app, KeyEvent::new(code, KeyModifiers::NONE))
}

fn say(app: &mut App, text: impl Into<String>) -> Effect {
    app.status = Some(text.into());
    Effect::None
}

/// Moves the cursor of whatever is on screen, or scrolls it, and stops at its ends.
fn travel(app: &mut App, by: isize) {
    let (rows, (height, total)) = {
        let seen = app.seen.borrow();
        (seen.rows, seen.page)
    };
    let (at, last) = match app.screen {
        Screen::Glance | Screen::AllCampaigns => (&mut app.row, rows.saturating_sub(1)),
        Screen::Campaign if app.pane == 0 => (&mut app.lane, rows.saturating_sub(1)),
        _ => (&mut app.scroll, total.saturating_sub(height)),
    };
    *at = at.saturating_add_signed(by).min(last);
}

fn reset(app: &mut App) {
    (app.row, app.lane, app.scroll) = (0, 0, 0);
}

/// Typing in the filter: Enter keeps it, Esc clears it.
fn filter(app: &mut App, code: KeyCode) -> Effect {
    match code {
        KeyCode::Enter => app.typing = false,
        KeyCode::Esc => {
            app.typing = false;
            app.filter.clear();
        }
        KeyCode::Backspace => {
            app.filter.pop();
        }
        KeyCode::Char(c) => app.filter.push(c),
        _ => return Effect::None,
    }
    reset(app);
    Effect::None
}

/// The keys every screen but the answer dialog shares, then the screen's own.
fn view(app: &mut App, code: KeyCode, ctrl: bool) -> Effect {
    let page = app.seen.borrow().page.0;
    let lists = matches!(app.screen, Screen::Glance | Screen::AllCampaigns)
        || (app.screen == Screen::Campaign && app.pane == 0);
    let half = if lists { 5 } else { (page / 2).max(1) } as isize;
    match code {
        // `q` closes before it quits.
        KeyCode::Char('q') if !app.close() => return Effect::Quit,
        KeyCode::Char('q') => {}
        // Esc clears a filter before it closes anything.
        KeyCode::Esc if !app.filter.is_empty() => {
            app.filter.clear();
            reset(app);
        }
        KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left => {
            if !app.close() && !app.filter.is_empty() {
                app.filter.clear();
                reset(app);
            }
        }
        KeyCode::Char('?') if app.screen == Screen::Help => {
            app.close();
        }
        KeyCode::Char('?') => app.go(Screen::Help),
        KeyCode::Char('m') => {
            app.mouse = !app.mouse;
            let now = if app.mouse {
                "mouse on: clicks select, the wheel scrolls"
            } else {
                "mouse off: drag selects text again"
            };
            app.status = Some(now.into());
            return Effect::Mouse(app.mouse);
        }
        KeyCode::Char('t') => {
            // Without colour there is nothing to cycle.
            if let Some(i) = app.themes.iter().position(|t| *t == app.theme) {
                app.theme = app.themes[(i + 1) % app.themes.len()];
            }
        }
        KeyCode::Char('r') => return Effect::Refresh,
        KeyCode::Char('/')
            if matches!(
                app.screen,
                Screen::Glance | Screen::Campaign | Screen::AllCampaigns
            ) =>
        {
            app.typing = true;
        }
        KeyCode::Char('j') | KeyCode::Down => travel(app, 1),
        KeyCode::Char('k') | KeyCode::Up => travel(app, -1),
        KeyCode::Char('g') | KeyCode::Home => travel(app, isize::MIN),
        KeyCode::Char('G') | KeyCode::End => travel(app, isize::MAX),
        KeyCode::Char('d') if ctrl => travel(app, half),
        KeyCode::Char('u') if ctrl => travel(app, -half),
        KeyCode::Char(' ') | KeyCode::PageDown if app.screen == Screen::Pager => {
            travel(app, page as isize)
        }
        KeyCode::PageUp if app.screen == Screen::Pager => travel(app, -(page as isize)),
        _ => {
            return match app.screen {
                Screen::Glance => glance_key(app, code),
                Screen::Campaign => campaign_key(app, code),
                Screen::AllCampaigns => all_key(app, code),
                Screen::Pager if code == KeyCode::Char('y') => {
                    app.status = Some("copied the document".into());
                    Effect::Copy(app.data.doc.body.clone())
                }
                _ => Effect::None,
            };
        }
    }
    Effect::None
}

/// Enter on a row with a pane: focus it when it is on this machine's Herdr, and say how
/// to get there when it is not. Nothing here can reach another machine's screen.
fn go_to(app: &mut App, host: &str, pane: &str) -> Effect {
    let g = &app.data.glance;
    if pane.is_empty() {
        return say(app, "no live pane for this row");
    }
    if host != g.caller_host {
        let host = ui::host_name(g, host);
        return say(
            app,
            format!("on {host} · pane {pane} · switch with prefix+w"),
        );
    }
    Effect::Run(Job::Focus {
        pane: pane.to_string(),
    })
}

/// `y`: the command that goes to the pane, in the footer as text and on the clipboard.
fn copy(app: &mut App, host: &str, pane: &str) -> Effect {
    if pane.is_empty() {
        return say(app, "nothing to copy: no live pane for this row");
    }
    let command = format!("herdr agent focus {pane}");
    let g = &app.data.glance;
    let place = if host == g.caller_host {
        String::new()
    } else {
        format!(" (run on {})", ui::host_name(g, host))
    };
    app.status = Some(format!("copied · {command}{place}"));
    Effect::Copy(command)
}

fn open(app: &mut App, root: i64, name: &str) -> Effect {
    // ponytail: one campaign, the one already read. `taskr campaign ID` (P1b) fetches
    // the selected one here.
    if root == 0 || app.data.campaign.root.id != root {
        return say(
            app,
            format!("! no campaign read for {name} yet: it needs taskr campaign (P1b)"),
        );
    }
    app.go(Screen::Campaign);
    (app.pane, app.lane) = (0, 0);
    Effect::None
}

fn report(app: &mut App) -> Effect {
    // ponytail: the one document in the data. `taskr doc get ID` fetches the row's own
    // once the campaign read lists them.
    if app.data.doc.body.is_empty() {
        return say(app, "no report recorded for this row");
    }
    app.go(Screen::Pager);
    Effect::None
}

fn start_answer(app: &mut App, need: Option<crate::model::Need>) -> Effect {
    match need {
        None => say(app, "a answers an ask: select one under NEEDS YOU"),
        Some(a) if app.sent.contains(&a.ask_id) => say(
            app,
            format!("ask {} is already answered from here", a.ask_id),
        ),
        Some(a) => {
            app.answer(a);
            Effect::None
        }
    }
}

fn park(app: &mut App, root: i64, name: &str, parked: bool) -> Effect {
    app.pending = Some(Job::Park {
        root,
        name: name.to_string(),
        park: !parked,
    });
    Effect::None
}

fn glance_key(app: &mut App, code: KeyCode) -> Effect {
    let view = app.view();
    let g = &view.data.glance;
    let (host, pane, root, name) = match glance::selected(&view) {
        Selected::Ask(a) => (&a.host, &a.pane_id, a.root_id, &a.campaign),
        Selected::Check(a) => (&a.host, &a.pane_id, a.root_id, &a.campaign),
        Selected::Campaign(c) => (&c.host, &c.pane_id, c.id, &c.name),
        Selected::Nothing if code == KeyCode::Char('c') => (&g.now, &g.now, 0, &g.now),
        Selected::Nothing => return Effect::None,
    };
    match code {
        KeyCode::Tab | KeyCode::BackTab => {
            let starts = glance::sections(g);
            let next = match code {
                KeyCode::Tab => starts.iter().find(|&&s| s > app.row).or(starts.first()),
                _ => starts
                    .iter()
                    .rev()
                    .find(|&&s| s < app.row)
                    .or(starts.last()),
            };
            app.row = next.copied().unwrap_or(0);
            // A section folded out of reach is not a place to land.
            travel(app, 0);
        }
        KeyCode::Enter => return go_to(app, host, pane),
        KeyCode::Char('y') => return copy(app, host, pane),
        KeyCode::Char('l') | KeyCode::Right => return open(app, root, name),
        KeyCode::Char('o') => return report(app),
        KeyCode::Char('c') => {
            app.go(Screen::AllCampaigns);
            app.row = 0;
        }
        KeyCode::Char('a') => {
            let need = match glance::selected(&view) {
                Selected::Ask(a) => Some(a.clone()),
                _ => None,
            };
            return start_answer(app, need);
        }
        KeyCode::Char('p') => {
            return match glance::selected(&view) {
                Selected::Campaign(c) => park(app, c.id, &c.name, c.parked),
                _ => say(app, "p parks a campaign: select one under CAMPAIGNS"),
            };
        }
        _ => {}
    }
    Effect::None
}

fn campaign_key(app: &mut App, code: KeyCode) -> Effect {
    let view = app.view();
    let (g, root) = (&view.data.glance, &view.data.campaign.root);
    let (host, pane, _) = campaign::target(&view).unwrap_or_default();
    let turn = |app: &mut App, pane: usize| (app.pane, app.scroll) = (pane, 0);
    match code {
        KeyCode::Tab => turn(app, (app.pane + 1) % 5),
        KeyCode::BackTab => turn(app, (app.pane + 4) % 5),
        KeyCode::Char(n @ '1'..='5') => turn(app, n as usize - '1' as usize),
        KeyCode::Enter if app.pane == 0 => return go_to(app, &host, &pane),
        // Docs: Enter reads.
        KeyCode::Enter if app.pane == 2 => return report(app),
        KeyCode::Char('o') => return report(app),
        KeyCode::Char('y') => return copy(app, &host, &pane),
        KeyCode::Char('a') => {
            let need = g
                .needs_you
                .iter()
                .find(|a| a.root_id == root.id && !app.sent.contains(&a.ask_id));
            return match need {
                Some(a) => start_answer(app, Some(a.clone())),
                None => say(app, "no open owner ask in this campaign"),
            };
        }
        KeyCode::Char('p') => {
            let parked = g.campaigns.iter().any(|c| c.id == root.id && c.parked);
            return park(app, root.id, &root.name, parked);
        }
        _ => {}
    }
    Effect::None
}

fn all_key(app: &mut App, code: KeyCode) -> Effect {
    let view = app.view();
    let Some(root) = lists::roots(&view).get(app.row).copied() else {
        return Effect::None;
    };
    let live = view
        .data
        .glance
        .campaigns
        .iter()
        .find(|c| c.id == root.id && root.status != "closed");
    match code {
        KeyCode::Enter => go_to(app, &root.host, live.map_or("", |c| &c.pane_id)),
        KeyCode::Char('l') | KeyCode::Right => open(app, root.id, &root.name),
        _ => Effect::None,
    }
}

/// Step 1: choose an option or write; Enter goes on to the confirmation and sends nothing.
fn answer(app: &mut App, code: KeyCode) -> Effect {
    let options = app
        .ask
        .as_ref()
        .map_or(0, |a| ask::parse(&a.text).options.len());
    match code {
        KeyCode::Esc => {
            app.close();
            return say(app, "cancelled, nothing was sent");
        }
        KeyCode::Enter if app.text.trim().is_empty() => return say(app, "write an answer first"),
        KeyCode::Enter => app.screen = Screen::Confirm,
        KeyCode::Tab | KeyCode::BackTab => app.editing = !app.editing,
        KeyCode::Down => app.choose((app.choice + 1).min(options)),
        KeyCode::Up => app.choose(app.choice.saturating_sub(1)),
        KeyCode::Backspace if app.editing => {
            app.text.pop();
        }
        // ponytail: the cursor stays at the end of the text; no editing mid-line.
        KeyCode::Char(c) if app.editing => app.text.push(c),
        KeyCode::Char('j') => app.choose((app.choice + 1).min(options)),
        KeyCode::Char('k') => app.choose(app.choice.saturating_sub(1)),
        KeyCode::Char('q') => {
            app.close();
            return say(app, "cancelled, nothing was sent");
        }
        _ => {}
    }
    Effect::None
}

/// Step 2: `y` sends. There is no default button, so Enter alone cannot.
fn confirm(app: &mut App, code: KeyCode) -> Effect {
    match (code, app.ask.clone()) {
        (KeyCode::Char('y'), Some(a)) => {
            app.close();
            app.sent.push(a.ask_id);
            app.status = Some(format!("sending the answer to ask {}", a.ask_id));
            Effect::Run(Job::Answer {
                ask_id: a.ask_id,
                text: app.text.trim().to_string(),
                prompt: !a.asker_waiting,
            })
        }
        (KeyCode::Char('e'), _) => {
            app.screen = Screen::Answer;
            Effect::None
        }
        (KeyCode::Esc | KeyCode::Char('q'), _) => {
            app.close();
            say(app, "cancelled, nothing was sent")
        }
        _ => Effect::None,
    }
}

/// The result of a job the loop ran.
pub fn done(app: &mut App, job: &Job, result: Result<(), String>) {
    app.status = match (job, result) {
        (Job::Focus { .. }, Ok(())) => None,
        (Job::Answer { ask_id, .. }, Ok(())) => Some(format!("✓ answered {ask_id}")),
        (Job::Park { name, park, .. }, Ok(())) => Some(format!(
            "✓ {} {name}",
            if *park { "parked" } else { "unparked" }
        )),
        (job, Err(error)) => {
            if let Job::Answer { ask_id, .. } = job {
                app.sent.retain(|id| id != ask_id);
            }
            Some(format!("! {} failed: {error}", job.label()))
        }
    };
}

/// The key a footer hint stands for.
fn hint_key(keys: &str) -> Option<KeyCode> {
    Some(match keys {
        "⏎" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "space" => KeyCode::Char(' '),
        "esc" => KeyCode::Esc,
        // `j k`, `g G`: the first of the pair. `1-5` is not one key.
        _ => KeyCode::Char(keys.chars().next().filter(|_| !keys.contains('-'))?),
    })
}

pub fn mouse(app: &mut App, event: MouseEvent, now: Instant) -> Effect {
    if app.pending.is_some() {
        return Effect::None;
    }
    let at = Position::new(event.column, event.row);
    // The last one drawn is on top.
    let hit = app
        .seen
        .borrow()
        .hits
        .iter()
        .rev()
        .find(|(area, _)| area.contains(at))
        .map(|(_, hit)| hit.clone());
    let pane = |app: &mut App, pane: usize| {
        if app.screen == Screen::Campaign && app.pane != pane {
            (app.pane, app.scroll) = (pane, 0);
        }
    };
    match event.kind {
        MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
            // ponytail: the wheel moves the cursor in a list rather than scrolling under
            // it, and its first notch over another pane only focuses that pane: how far
            // the pane scrolls is known once it has been drawn focused.
            let over = match hit {
                Some(Hit::Pane(i)) => i,
                Some(Hit::Lane(_)) => 0,
                _ => app.pane,
            };
            if app.screen == Screen::Campaign && over != app.pane {
                pane(app, over);
                return Effect::None;
            }
            let down = event.kind == MouseEventKind::ScrollDown;
            press(app, if down { KeyCode::Down } else { KeyCode::Up })
        }
        MouseEventKind::Down(MouseButton::Left) => {
            let Some(hit) = hit else {
                return Effect::None;
            };
            let double = app.click.take().is_some_and(|(then, last)| {
                last == hit && now.saturating_duration_since(then) < DOUBLE_CLICK
            });
            if !double {
                app.click = Some((now, hit.clone()));
            }
            match hit {
                Hit::Key(keys) => {
                    return hint_key(&keys).map_or(Effect::None, |code| press(app, code));
                }
                Hit::Pane(i) => pane(app, i),
                Hit::Row(i) => app.row = i,
                Hit::Lane(i) => {
                    pane(app, 0);
                    app.lane = i;
                }
            }
            // A double-click on a row is Enter.
            if double && !matches!(hit, Hit::Pane(_)) {
                press(app, KeyCode::Enter)
            } else {
                Effect::None
            }
        }
        // The right button stays with Herdr's pane menu.
        _ => Effect::None,
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::{draw, frames};

    /// The campaign row under the cursor: id, name, pane, parked.
    struct Row {
        id: i64,
        name: String,
        pane_id: String,
    }

    fn campaign(app: &App) -> Row {
        match glance::selected(app) {
            Selected::Campaign(c) => Row {
                id: c.id,
                name: c.name.clone(),
                pane_id: c.pane_id.clone(),
            },
            _ => panic!("the cursor is not on a campaign row"),
        }
    }

    /// The loop without a terminal: draw, then one event, as `main` does.
    struct Pane {
        app: App,
        terminal: Terminal<TestBackend>,
        now: Instant,
    }

    impl Pane {
        fn new(width: u16, height: u16) -> Self {
            let mut app = App::new(frames::fixture());
            app.fetch.loaded = true;
            let terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            Self {
                app,
                terminal,
                now: Instant::now(),
            }
        }

        fn text(&mut self) -> String {
            self.terminal.draw(|f| draw(f, &self.app)).unwrap();
            frames::text(self.terminal.backend().buffer())
        }

        fn code(&mut self, code: KeyCode) -> Effect {
            self.text();
            press(&mut self.app, code)
        }

        /// Each character as a key; the last key's effect.
        fn keys(&mut self, keys: &str) -> Effect {
            let mut last = Effect::None;
            for c in keys.chars() {
                last = self.code(match c {
                    '\n' => KeyCode::Enter,
                    '\t' => KeyCode::Tab,
                    '\x1b' => KeyCode::Esc,
                    c => KeyCode::Char(c),
                });
            }
            last
        }

        fn mouse(&mut self, kind: MouseEventKind, column: u16, row: u16) -> Effect {
            self.text();
            self.now += Duration::from_millis(100);
            let event = MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            };
            mouse(&mut self.app, event, self.now)
        }

        fn click(&mut self, column: u16, row: u16) -> Effect {
            self.mouse(MouseEventKind::Down(MouseButton::Left), column, row)
        }

        /// Clicks the first screen line containing `text`, at its first column.
        fn click_on(&mut self, text: &str) -> Effect {
            let screen = self.text();
            let (y, line) = screen
                .lines()
                .enumerate()
                .find(|(_, l)| l.contains(text))
                .unwrap_or_else(|| panic!("{text:?} is not on screen:\n{screen}"));
            let x = line[..line.find(text).unwrap()].chars().count();
            self.click(x as u16, y as u16)
        }

        fn status(&self) -> &str {
            self.app.status.as_deref().unwrap_or("")
        }
    }

    #[test]
    fn the_cursor_moves_and_stops_at_the_ends() {
        let mut p = Pane::new(46, 30);
        assert_eq!(p.keys("k"), Effect::None);
        assert_eq!(p.app.row, 0);
        p.keys("jj");
        assert_eq!(p.app.row, 2);
        p.keys("G");
        let last = p.app.row;
        assert!(last > 5, "{last}");
        p.keys("jjj");
        assert_eq!(p.app.row, last);
        p.keys("g");
        assert_eq!(p.app.row, 0);
        // Tab goes section to section and wraps: asks, checks, campaigns.
        p.keys("\t");
        assert_eq!(p.app.row, 2);
        p.keys("\t");
        assert_eq!(p.app.row, 5);
        p.keys("\t");
        assert_eq!(p.app.row, 0);
        p.code(KeyCode::BackTab);
        assert_eq!(p.app.row, 5);
        // At 20 rows the folded campaigns are out of reach.
        let mut short = Pane::new(46, 20);
        short.keys("G");
        assert!(short.app.row < last);
        assert!(matches!(
            glance::selected(&short.app),
            Selected::Campaign(_)
        ));
    }

    #[test]
    fn enter_focuses_a_local_pane_and_only_hints_at_a_remote_one() {
        let mut p = Pane::new(46, 30);
        // The first ask is on this machine, the second on another.
        let pane = p.app.data.glance.needs_you[0].pane_id.clone();
        assert_eq!(p.keys("\n"), Effect::Run(Job::Focus { pane: pane.clone() }));
        assert_eq!(
            p.keys("y"),
            Effect::Copy(format!("herdr agent focus {pane}"))
        );
        assert!(p.text().contains("copied · herdr agent focus"));
        assert_eq!(p.keys("j\n"), Effect::None);
        assert!(
            p.status().starts_with("on laptop · pane "),
            "{}",
            p.status()
        );
        assert!(p.status().ends_with("switch with prefix+w"));
        // A failed focus says so, and any key clears the line.
        done(
            &mut p.app,
            &Job::Focus { pane },
            Err("herdr: pane not found".into()),
        );
        assert!(p.text().contains("! go to"));
        p.keys("k");
        assert_eq!(p.status(), "");
    }

    #[test]
    fn an_answer_is_sent_only_by_y_on_the_confirm_step() {
        let mut p = Pane::new(120, 40);
        let ask = p.app.data.glance.needs_you[0].clone();
        assert!(ask.asker_waiting);
        // Enter reviews; Enter again, and every key but y, sends nothing.
        assert_eq!(p.keys("a\n\n x\tj"), Effect::None);
        assert_eq!(p.app.screen, Screen::Confirm);
        assert!(p.text().contains("You are about to send this answer"));
        // A snapshot under the dialog cannot change which ask is answered.
        let mut next = p.app.data.glance.clone();
        next.needs_you.remove(0);
        p.app.snapshot(next);
        let text = p.app.text.clone();
        assert!(text.starts_with("A: "), "{text}");
        let sent = p.keys("y");
        assert_eq!(
            sent,
            Effect::Run(Job::Answer {
                ask_id: ask.ask_id,
                text,
                prompt: false
            })
        );
        assert_eq!(p.app.screen, Screen::Glance);
        done(
            &mut p.app,
            &Job::Answer {
                ask_id: ask.ask_id,
                text: String::new(),
                prompt: false,
            },
            Ok(()),
        );
        assert!(p.text().contains(&format!("✓ answered {}", ask.ask_id)));
    }

    #[test]
    fn the_answer_can_be_chosen_written_and_cancelled() {
        let mut p = Pane::new(46, 30);
        // The second ask's asker is not waiting: the answer also goes to its pane.
        p.app.data.glance.needs_you[1].asker_waiting = false;
        let ask = p.app.data.glance.needs_you[1].clone();
        p.keys("ja");
        assert_eq!(p.app.screen, Screen::Answer);
        let first = p.app.text.clone();
        p.keys("j");
        assert_ne!(p.app.text, first);
        assert!(p.app.text.starts_with("B: "));
        // Tab edits the text: letters are typed, not commands.
        p.keys("\t, jk");
        p.code(KeyCode::Backspace);
        assert!(p.app.text.ends_with(", j"), "{}", p.app.text);
        let text = p.app.text.clone();
        // e goes back to the text; esc from there sends nothing.
        p.keys("\ne");
        assert_eq!(p.app.screen, Screen::Answer);
        assert_eq!(
            p.keys("\ny"),
            Effect::Run(Job::Answer {
                ask_id: ask.ask_id,
                text,
                prompt: true
            })
        );
        // It cannot be sent twice from here while the row is still listed.
        p.keys("a");
        assert_eq!(p.app.screen, Screen::Glance);
        assert!(p.status().contains("already answered"));

        // "write your own answer" starts empty, and an empty answer is not reviewed.
        let mut p = Pane::new(46, 30);
        p.keys("a");
        p.code(KeyCode::Down);
        p.code(KeyCode::Down);
        p.code(KeyCode::Down);
        assert_eq!((p.app.text.as_str(), p.app.editing), ("", true));
        p.keys("\n");
        assert_eq!(p.app.screen, Screen::Answer);
        p.keys("ok\n");
        assert_eq!(p.app.screen, Screen::Confirm);
        assert_eq!(p.keys("\x1b"), Effect::None);
        assert_eq!(
            (p.app.screen, p.status()),
            (Screen::Glance, "cancelled, nothing was sent")
        );
        // On a campaign row there is no ask to answer.
        p.keys("Ga");
        assert_eq!(p.app.screen, Screen::Glance);
    }

    #[test]
    fn park_asks_first_and_is_for_campaign_rows_only() {
        let mut p = Pane::new(46, 30);
        assert_eq!(p.keys("p"), Effect::None);
        assert!(p.app.pending.is_none() && p.status().contains("parks a campaign"));
        p.keys("\t\t");
        let c = campaign(&p.app);
        let job = Job::Park {
            root: c.id,
            name: c.name.clone(),
            park: true,
        };
        p.keys("p");
        assert!(p.text().contains(&format!("park {}?", c.name)));
        // Any key but y cancels, and that key does nothing else.
        assert_eq!(p.keys("j"), Effect::None);
        assert!(p.app.pending.is_none() && p.app.row == 5);
        assert_eq!(p.keys("py"), Effect::Run(job.clone()));
        done(&mut p.app, &job, Ok(()));
        assert!(p.status().starts_with("✓ parked"));
        // The parked row at the end unparks.
        p.keys("G");
        assert!(matches!(
            p.keys("py"),
            Effect::Run(Job::Park { park: false, .. })
        ));
    }

    #[test]
    fn the_filter_narrows_rows_and_never_the_pill() {
        let mut p = Pane::new(46, 30);
        let pill = |p: &mut Pane| p.text().lines().next().unwrap().contains("2 need you");
        assert!(pill(&mut p));
        // Letters go to the filter while it is open, q included.
        assert_eq!(p.keys("/doq"), Effect::None);
        assert_eq!(p.app.filter, "doq");
        p.code(KeyCode::Backspace);
        p.keys("c");
        let screen = p.text();
        assert!(screen.contains("filter: doc") && screen.contains("docs-refresh"));
        assert!(!screen.contains("search-index") && pill(&mut p));
        // Enter keeps the filter, and the keys work on what is left.
        p.keys("\nG");
        let c = campaign(&p.app.view());
        assert_eq!(c.name, "docs-refresh");
        p.keys("\x1b");
        assert!(p.app.filter.is_empty() && p.text().contains("search-index"));
        // Esc while typing clears it too.
        p.keys("/x\x1b");
        assert!(p.app.filter.is_empty() && !p.app.typing);
    }

    #[test]
    fn levels_panes_overlays_and_quit() {
        let mut p = Pane::new(120, 40);
        // Only the campaign that has been read opens.
        p.keys("\t\tl");
        assert_eq!(p.app.screen, Screen::Glance);
        assert!(p.status().contains("no campaign read"));
        let root = p.app.data.campaign.root.id;
        let ids = glance::ids(&p.app.data.glance);
        p.app.row = ids
            .iter()
            .position(|r| *r == glance::RowId::Campaign(root))
            .unwrap();
        p.keys("l");
        assert_eq!(p.app.screen, Screen::Campaign);
        p.keys("jj3");
        assert_eq!((p.app.lane, p.app.pane), (2, 2));
        // Docs is longer than its box: it scrolls and stops at its end.
        p.keys("jjjjjjjjjjjjjjjjjjjj");
        let (height, total) = p.app.seen.borrow().page;
        assert!(
            total > height && p.app.scroll == total - height,
            "{}",
            p.app.scroll
        );
        p.keys("\t");
        assert_eq!((p.app.pane, p.app.scroll), (3, 0));
        p.code(KeyCode::BackTab);
        p.keys("1/arch\n");
        assert!(p.text().contains("filter: arch"));
        p.keys("G");
        let rows = p.app.seen.borrow().rows;
        assert!(p.app.lane + 1 == rows && rows < 12, "{rows}");
        // o reads, space pages, q closes one level at a time and quits only on the glance.
        p.keys("\x1bo");
        assert_eq!(p.app.screen, Screen::Pager);
        p.keys(" ");
        assert!(p.app.scroll > 0);
        assert_eq!(p.keys("y"), Effect::Copy(p.app.data.doc.body.clone()));
        p.keys("?");
        assert_eq!(p.app.screen, Screen::Help);
        assert!(p.text().contains("Keys · pager"));
        assert_eq!(p.keys("qqq"), Effect::None);
        assert_eq!(p.app.screen, Screen::Glance);
        assert_eq!(p.keys("c"), Effect::None);
        assert_eq!(p.app.screen, Screen::AllCampaigns);
        p.keys("G");
        assert_eq!(p.app.row + 1, p.app.data.roots.len());
        p.keys("/tui\n");
        assert_eq!(p.keys("l"), Effect::None);
        assert_eq!(p.app.screen, Screen::Campaign);
        p.keys("hh");
        assert_eq!(p.app.screen, Screen::Glance);
        assert_eq!(p.keys("q"), Effect::Quit);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        p.keys("a");
        assert_eq!(key(&mut p.app, ctrl_c), Effect::Quit);
    }

    #[test]
    fn theme_mouse_and_refresh_keys() {
        let mut p = Pane::new(46, 30);
        assert_eq!(p.keys("r"), Effect::Refresh);
        assert_eq!(p.keys("m"), Effect::Mouse(false));
        assert_eq!(p.keys("m"), Effect::Mouse(true));
        p.keys("t");
        assert_eq!(p.app.theme, crate::theme::LIGHT);
        p.keys("tt");
        assert_eq!(p.app.theme, crate::theme::DARK);
        p.app.theme = crate::theme::PLAIN;
        p.keys("t");
        assert_eq!(p.app.theme, crate::theme::PLAIN);
    }

    #[test]
    fn clicks_select_double_clicks_go_and_the_wheel_moves() {
        let mut p = Pane::new(46, 30);
        assert_eq!(p.click_on("docs-refresh"), Effect::None);
        let c = campaign(&p.app);
        assert_eq!(c.name, "docs-refresh");
        // A second click soon after is Enter; a slow one is only a click.
        let pane = c.pane_id.clone();
        let again = p.click_on("docs-refresh");
        assert!(
            again == Effect::Run(Job::Focus { pane }) || !p.status().is_empty(),
            "{again:?}"
        );
        p.now += DOUBLE_CLICK;
        p.app.status = None;
        assert_eq!(p.click_on("docs-refresh"), Effect::None);
        assert_eq!(p.status(), "");
        let row = p.app.row;
        p.mouse(MouseEventKind::ScrollDown, 5, 5);
        p.mouse(MouseEventKind::ScrollDown, 5, 5);
        p.mouse(MouseEventKind::ScrollUp, 5, 5);
        assert_eq!(p.app.row, row + 1);
        // The right button is Herdr's.
        assert_eq!(
            p.mouse(MouseEventKind::Down(MouseButton::Right), 5, 5),
            Effect::None
        );
        assert_eq!(p.app.row, row + 1);
        // A footer hint is its key.
        p.keys("g");
        p.click_on("answer");
        assert_eq!(p.app.screen, Screen::Answer);
        p.click_on("esc");
        assert_eq!(p.app.screen, Screen::Glance);

        // Campaign view: tabs when narrow; panes, lanes and the wheel when wide.
        let root = p.app.data.campaign.root.id;
        let ids = glance::ids(&p.app.data.glance);
        p.app.row = ids
            .iter()
            .position(|r| *r == glance::RowId::Campaign(root))
            .unwrap();
        p.keys("l");
        p.click_on("Docs");
        assert_eq!(p.app.pane, 2);
        let mut wide = Pane::new(120, 40);
        wide.app.row = p.app.row;
        wide.keys("l");
        wide.click_on("[5] Log");
        assert_eq!(wide.app.pane, 4);
        wide.click_on("arch-plan-counter2");
        assert_eq!((wide.app.pane, wide.app.lane), (0, 6));
        let screen = wide.text();
        let y = screen.lines().position(|l| l.contains("[3] Docs")).unwrap() as u16;
        // The first notch focuses the pane under the pointer; the next ones scroll it.
        wide.mouse(MouseEventKind::ScrollDown, 100, y + 2);
        assert_eq!((wide.app.pane, wide.app.scroll), (2, 0));
        wide.mouse(MouseEventKind::ScrollDown, 100, y + 2);
        assert_eq!((wide.app.pane, wide.app.scroll), (2, 1));
        // Under a dialog nothing of the glance can be clicked.
        let mut wide = Pane::new(120, 40);
        wide.keys("a");
        assert_eq!(wide.click_on("docs-refresh"), Effect::None);
        assert_eq!((wide.app.screen, wide.app.row), (Screen::Answer, 0));
    }
}
