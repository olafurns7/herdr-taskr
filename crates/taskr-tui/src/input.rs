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
    actions::{Done, Job},
    ask, campaign,
    glance::{self, Selected},
    lists,
    model::Doc,
    slotr, ui,
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
    /// Clear the screen and paint it whole (Ctrl-L).
    Redraw,
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
    if ctrl && key.code == KeyCode::Char('l') {
        return Effect::Redraw;
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
        Screen::Answer => answer(app, key.code, ctrl),
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

/// The last frame drew the glance's detail pane (wide, with a row selected).
fn detail_drawn(app: &App) -> bool {
    app.seen
        .borrow()
        .hits
        .iter()
        .any(|(_, h)| *h == Hit::Detail)
}

/// The glance's detail pane has the keys: it is focused and on screen.
fn on_detail(app: &App) -> bool {
    app.screen == Screen::Glance && app.detail && detail_drawn(app)
}

/// Puts the glance cursor on `row`; the detail pane then shows another row from its top.
fn select(app: &mut App, row: usize) {
    if app.row != row {
        (app.row, app.scroll) = (row, 0);
    }
}

/// Moves the cursor of whatever is on screen, or scrolls it, and stops at its ends.
fn travel(app: &mut App, by: isize) {
    let (rows, (height, total)) = {
        let seen = app.seen.borrow();
        (seen.rows, seen.page)
    };
    if app.screen == Screen::Glance && !on_detail(app) {
        let row = app
            .row
            .saturating_add_signed(by)
            .min(rows.saturating_sub(1));
        return select(app, row);
    }
    let (at, last) = match app.screen {
        Screen::AllCampaigns | Screen::Slotr => (&mut app.row, rows.saturating_sub(1)),
        Screen::Campaign if app.pane == 0 => (&mut app.lane, rows.saturating_sub(1)),
        _ => (&mut app.scroll, total.saturating_sub(height)),
    };
    *at = at.saturating_add_signed(by).min(last);
}

fn scroll(app: &mut App, by: isize) -> Effect {
    travel(app, by);
    Effect::None
}

fn reset(app: &mut App) {
    (app.row, app.lane, app.scroll) = (0, 0, 0);
    // A new filter starts a fresh walk of the list.
    app.detail = false;
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
    let detail = on_detail(app);
    let lists = (app.screen == Screen::Glance && !detail)
        || matches!(app.screen, Screen::AllCampaigns | Screen::Slotr)
        || (app.screen == Screen::Campaign && app.pane == 0);
    let half = if lists { 5 } else { (page / 2).max(1) } as isize;
    match code {
        // `q` closes before it quits.
        KeyCode::Char('q') if !app.close() => return Effect::Quit,
        KeyCode::Char('q') => {}
        // Back from the detail pane to the row it shows.
        KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left if detail => app.detail = false,
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
        KeyCode::Char('s') if app.screen == Screen::Slotr => {
            app.close();
        }
        KeyCode::Char('s') if matches!(app.screen, Screen::Glance | Screen::Campaign) => {
            app.go(Screen::Slotr);
            app.row = 0;
        }
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
        KeyCode::Char(' ') if app.screen == Screen::Pager => travel(app, page as isize),
        KeyCode::PageDown if app.screen == Screen::Pager || detail => travel(app, page as isize),
        KeyCode::PageUp if app.screen == Screen::Pager || detail => travel(app, -(page as isize)),
        _ => {
            return match app.screen {
                Screen::Glance => glance_key(app, code),
                Screen::Campaign => campaign_key(app, code),
                Screen::AllCampaigns => all_key(app, code),
                Screen::Slotr => slotr_key(app, code),
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
/// to get there when it is not, or when the snapshot does not say whose Herdr the pane is
/// on (a taskr older than P1b). Pane ids repeat across machines, so a guess could focus
/// someone else's pane. Nothing here can reach another machine's screen.
fn go_to(app: &mut App, host: &str, pane: &str) -> Effect {
    let g = &app.data.glance;
    if pane.is_empty() {
        return say(app, "no live pane for this row");
    }
    if !g.local(host) {
        let host = match ui::host_name(g, host) {
            name if name.is_empty() => "the hub".to_string(),
            name => name,
        };
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
    let place = match ui::host_name(g, host) {
        _ if g.local(host) => String::new(),
        name if name.is_empty() => " (run on the hub)".to_string(),
        name => format!(" (run on {name})"),
    };
    app.status = Some(format!("copied · {command}{place}"));
    Effect::Copy(command)
}

/// `l`: read the campaign and show it. One that is already read shows at once and is
/// refreshed behind the view.
fn open(app: &mut App, root: i64, name: &str) -> Effect {
    if root == 0 {
        return say(app, "this row has no campaign to open");
    }
    let loaded = app.data.campaign.root.id == root;
    if loaded {
        app.go(Screen::Campaign);
        (app.pane, app.lane) = (0, 0);
    } else {
        app.status = Some(format!("reading {name}"));
    }
    Effect::Run(Job::Campaign {
        root,
        name: name.to_string(),
        open: !loaded,
    })
}

/// `o`: read the document under the cursor, or the selected lane's report.
fn report(app: &mut App) -> Effect {
    match campaign::doc(&app.view()) {
        Some(row) if row.captured => Effect::Run(Job::Doc { row: row.clone() }),
        Some(_) => say(
            app,
            "that document was registered but its text was never kept",
        ),
        None => say(app, "no report recorded for this lane"),
    }
}

/// The campaign read to repeat after a new snapshot: the one on screen, if any.
pub fn follow(app: &App) -> Option<Job> {
    let showing =
        app.screen == Screen::Campaign || app.back.iter().any(|b| b.screen == Screen::Campaign);
    let root = &app.data.campaign.root;
    showing.then(|| Job::Campaign {
        root: root.id,
        name: root.name.clone(),
        open: false,
    })
}

/// `--demo`: reads come from the fixture, and nothing else runs.
pub fn demo(app: &mut App, job: Job) {
    let result = match &job {
        Job::Campaign { root, .. } if *root == app.data.campaign.root.id => {
            Ok(Done::Campaign(Box::new(app.data.campaign.clone())))
        }
        Job::Campaign { .. } => Err(format!(
            "the demo has one campaign, {}",
            app.data.campaign.root.name
        )),
        Job::Doc { .. } => Ok(Done::Doc(app.data.doc.body.clone())),
        Job::Slotr => Ok(Done::Slotr(Box::new(app.data.slotr.clone()))),
        _ => {
            app.status = Some(format!("demo: {} (nothing was sent)", job.label()));
            return;
        }
    };
    done(app, &job, result);
}

/// Pasted text is text, never keys: it goes to the field being typed in, or nowhere.
pub fn paste(app: &mut App, text: &str) {
    let line: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if app.typing {
        app.filter.push_str(line.trim());
        reset(app);
    } else if app.screen == Screen::Answer && app.editing {
        app.text.push_str(&line);
    }
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
            // Sections, then the detail pane when it is drawn, then round again.
            let starts = glance::sections(g);
            let (tab, detail) = (code == KeyCode::Tab, on_detail(app));
            let next = match (detail, tab) {
                (true, true) => starts.first(),
                (true, false) => starts.last(),
                (false, true) => starts.iter().find(|&&s| s > app.row),
                (false, false) => starts.iter().rev().find(|&&s| s < app.row),
            };
            if next.is_none() && !detail && detail_drawn(app) {
                app.detail = true;
                return Effect::None;
            }
            let wrap = if tab { starts.first() } else { starts.last() };
            app.detail = false;
            select(app, next.or(wrap).copied().unwrap_or(0));
            // A section folded out of reach is not a place to land.
            travel(app, 0);
        }
        KeyCode::Enter => return go_to(app, host, pane),
        KeyCode::Char('y') => return copy(app, host, pane),
        KeyCode::Char('l') | KeyCode::Right => return open(app, root, name),
        KeyCode::Char('o') => return say(app, "l opens the campaign; its reports are read there"),
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

/// The slotr view: ⏎ and `y` work the row's pane, as on every list; `l` opens the row's
/// campaign, by the root taskr found for its task or else by the campaign's name.
fn slotr_key(app: &mut App, code: KeyCode) -> Effect {
    let view = app.view();
    let Some(row) = slotr::rows(&view.data.slotr).get(app.row).copied() else {
        return Effect::None;
    };
    let host = &view.data.slotr.host;
    match code {
        KeyCode::Enter => go_to(app, host, &row.pane),
        KeyCode::Char('y') => copy(app, host, &row.pane),
        KeyCode::Char('l') | KeyCode::Right => {
            let named = view
                .data
                .glance
                .campaigns
                .iter()
                .find(|c| !row.campaign.is_empty() && c.name == row.campaign);
            match (row.root_id, named) {
                (0, Some(c)) => open(app, c.id, &c.name),
                (0, None) => say(app, "no campaign for this row"),
                (root, _) => {
                    let name = if row.root_name.is_empty() {
                        &row.campaign
                    } else {
                        &row.root_name
                    };
                    open(app, root, name)
                }
            }
        }
        _ => Effect::None,
    }
}

/// Step 1: choose an option or write; Enter goes on to the confirmation and sends nothing.
fn answer(app: &mut App, code: KeyCode, ctrl: bool) -> Effect {
    // The ask's own text scrolls when it is longer than its room.
    let half = (app.seen.borrow().page.0 / 2).max(1) as isize;
    match code {
        KeyCode::Char('d') if ctrl => return scroll(app, half),
        KeyCode::Char('u') if ctrl => return scroll(app, -half),
        KeyCode::PageDown => return scroll(app, 2 * half),
        KeyCode::PageUp => return scroll(app, -2 * half),
        _ => {}
    }
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
pub fn done(app: &mut App, job: &Job, result: Result<Done, String>) {
    let showing =
        app.screen == Screen::Campaign || app.back.iter().any(|b| b.screen == Screen::Campaign);
    let note = |done: &Done| match done {
        Done::Note(note) => format!(" · {note}"),
        _ => String::new(),
    };
    if let Job::Slotr = job {
        // The slotr view says its own trouble, on its own line; the last good pools stay.
        let s = &mut app.slotr;
        match result {
            Ok(Done::Slotr(slotr)) if slotr.available => {
                app.data.slotr = *slotr;
                (s.loaded, s.error) = (true, None);
            }
            Ok(Done::Slotr(slotr)) if !slotr.error.is_empty() => s.error = Some(slotr.error),
            Ok(_) => s.error = Some("slotr unavailable".into()),
            Err(error) => s.error = Some(error),
        }
        return;
    }
    app.status = match (job, result) {
        (Job::Campaign { root, open, .. }, Ok(Done::Campaign(campaign))) => {
            if *open && !showing {
                app.data.campaign = *campaign;
                app.go(Screen::Campaign);
                (app.pane, app.lane) = (0, 0);
            } else if showing && app.data.campaign.root.id == *root {
                // A refresh under the view: the cursor stays where it can.
                app.lane = app.lane.min(campaign.lanes.len());
                app.data.campaign = *campaign;
            }
            None
        }
        (Job::Doc { row }, Ok(Done::Doc(body))) => {
            app.data.doc = Doc {
                id: row.id,
                kind: row.kind.clone(),
                name: row.name.clone(),
                lane: row.lane.clone(),
                version: row.version,
                body,
            };
            if app.screen != Screen::Pager {
                app.go(Screen::Pager);
            }
            None
        }
        // Recorded is recorded, even when the prompt did not reach the asker's pane: the
        // ask stays out of reach here, and the line says what is left to do.
        (Job::Answer { ask_id, .. }, Ok(done)) => {
            Some(format!("✓ answered {ask_id}{}", note(&done)))
        }
        (Job::Park { name, park, .. }, Ok(_)) => Some(format!(
            "✓ {} {name}",
            if *park { "parked" } else { "unparked" }
        )),
        (_, Ok(_)) => None,
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
            // On the glance the wheel works the pane under it: the list or the detail.
            match hit {
                Some(Hit::Detail) => app.detail = true,
                Some(Hit::Row(_)) => app.detail = false,
                _ => {}
            }
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
                Hit::Detail => app.detail = true,
                Hit::Row(i) => {
                    app.detail = false;
                    if app.screen == Screen::Glance {
                        select(app, i);
                    } else {
                        app.row = i;
                    }
                }
                Hit::Lane(i) => {
                    pane(app, 0);
                    app.lane = i;
                }
            }
            // A double-click on a row is Enter.
            if double && !matches!(hit, Hit::Pane(_) | Hit::Detail) {
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

        /// One key. A read it asks for is served from the fixture, as `--demo` does.
        fn code(&mut self, code: KeyCode) -> Effect {
            self.text();
            match press(&mut self.app, code) {
                Effect::Run(job) if job.reads() => {
                    demo(&mut self.app, job);
                    Effect::None
                }
                effect => effect,
            }
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
            Ok(Done::Ok),
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
        done(&mut p.app, &job, Ok(Done::Ok));
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
        assert!(
            p.status().contains("the demo has one campaign"),
            "{}",
            p.status()
        );
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
        // Docs has a cursor that stops on the last document; Enter reads it.
        p.keys("jjjjjjjjjjjjjjjjjjjj");
        let docs = p.app.data.campaign.docs.clone();
        assert_eq!(p.app.scroll, docs.len() - 1);
        p.keys("\n");
        assert_eq!(
            (p.app.screen, p.app.data.doc.id),
            (Screen::Pager, docs[docs.len() - 1].id)
        );
        p.keys("q");
        assert_eq!((p.app.screen, p.app.pane), (Screen::Campaign, 2));
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

    /// A glance as a taskr older than P1b sends it: no server name, no panes, no sparks,
    /// and a check that belongs to no campaign.
    fn old_taskr(p: &mut Pane) {
        let g = &mut p.app.data.glance;
        g.server_host.clear();
        g.needs_you[0].pane_id.clear();
        g.needs_you[0].host.clear();
        g.attention[0].campaign.clear();
        g.attention[0].kind = "daemon_unhealthy".into();
        g.attention[0].root_id = 0;
        g.campaigns.iter_mut().for_each(|c| c.spark.clear());
    }

    #[test]
    fn enter_is_offered_only_where_it_can_go() {
        let mut p = Pane::new(46, 30);
        let before = p.text();
        assert!(before.lines().nth(3).unwrap().contains('⏎') && before.contains("⏎ go"));
        old_taskr(&mut p);
        let screen = p.text();
        // The ask has no pane: no mark on its row, no hint, and Enter says why.
        assert!(!screen.lines().nth(3).unwrap().contains('⏎'), "{screen}");
        assert!(!screen.lines().last().unwrap().contains('⏎') && screen.contains("a answer"));
        assert_eq!(p.keys("\n"), Effect::None);
        assert_eq!(p.status(), "no live pane for this row");
        // A check for no campaign is named for what it is about, and opens nothing.
        assert!(screen.contains(" ! daemon"), "{screen}");
        p.keys("jj");
        assert!(!p.text().lines().last().unwrap().contains("l open"));
        // Without sparks the names take the room.
        assert!(screen.contains("search-index "));
        // A hub row with a pane, when the snapshot does not name its server: whose Herdr
        // the pane is on is unknown, so nothing is focused.
        let lead = glance::ids(&p.app.data.glance)
            .iter()
            .position(|r| matches!(r, glance::RowId::Campaign(_)))
            .unwrap();
        p.app.row = lead;
        assert!(!campaign(&p.app).pane_id.is_empty());
        assert_eq!(p.keys("\n"), Effect::None);
        assert!(
            p.status().starts_with("on the hub · pane ")
                && p.status().ends_with("switch with prefix+w"),
            "{}",
            p.status()
        );
        assert!(
            p.text().contains("how to get there")
                || !p.text().lines().last().unwrap().contains("⏎ go")
        );
    }

    #[test]
    fn a_snapshot_marks_the_campaigns_that_changed() {
        let mut p = Pane::new(46, 30);
        p.app.changed.clear();
        let mut next = p.app.data.glance.clone();
        p.app.snapshot(next.clone());
        assert!(
            p.app.changed.is_empty(),
            "the same snapshot changes nothing"
        );
        // A new event on one, a lane more on another, and a campaign that was not there.
        let (a, b) = (next.campaigns[0].id, next.campaigns[1].id);
        next.campaigns[0].last.get_or_insert_default().event_id += 1;
        next.campaigns[1].lanes.working += 1;
        let mut new = next.campaigns[2].clone();
        (new.id, new.name) = (9999, "brand-new".into());
        next.campaigns.push(new);
        p.app.snapshot(next.clone());
        assert_eq!(p.app.changed, [a, b, 9999]);
        // The marks stay through the next snapshot and go with any key.
        p.app.snapshot(next);
        assert_eq!(p.app.changed.len(), 3);
        assert!(p.text().lines().any(|l| l.starts_with('•')));
        p.keys("j");
        assert!(p.app.changed.is_empty());
        // The first snapshot of a session marks nothing: there was nothing to compare.
        let mut first = App::new(crate::model::Data::default());
        first.snapshot(p.app.data.glance.clone());
        assert!(first.changed.is_empty());
        // The all-campaigns list is built from the glance: open campaigns, then quiet ones.
        let g = &p.app.data.glance;
        assert_eq!(
            first.data.roots.len(),
            g.campaigns.len() + g.quiet.names.len()
        );
        assert_eq!(
            first.data.roots.last().unwrap().id,
            *g.quiet.root_ids.last().unwrap()
        );
    }

    #[test]
    fn a_long_ask_can_be_read_before_it_is_answered() {
        let mut p = Pane::new(46, 30);
        let mut ask = p.app.data.glance.needs_you[0].clone();
        let body: Vec<String> = (1..=60)
            .map(|i| format!("Paragraph {i} of the case for and against."))
            .collect();
        ask.text = format!("{} LASTWORD (A) yes; (B) no", body.join(" "));
        p.app.answer(ask);
        let top = p.text();
        assert!(
            top.contains("Paragraph 1 of")
                && !top.contains("LASTWORD")
                && top.contains("ctrl-d ctrl-u scroll")
        );
        let ctrl = |p: &mut Pane, c| {
            p.text();
            key(
                &mut p.app,
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL),
            )
        };
        ctrl(&mut p, 'd');
        assert!(p.app.scroll > 0 && !p.text().contains("Paragraph 1 of"));
        for _ in 0..40 {
            ctrl(&mut p, 'd');
        }
        let bottom = p.text();
        assert!(
            bottom.contains("LASTWORD") && bottom.contains("A  yes"),
            "{bottom}"
        );
        // While the answer is being edited, ctrl-d still scrolls and types nothing.
        p.keys("\t");
        let text = p.app.text.clone();
        ctrl(&mut p, 'u');
        ctrl(&mut p, 'd');
        assert_eq!(p.app.text, text);
        p.code(KeyCode::PageUp);
        assert!(!p.text().contains("LASTWORD"));
    }

    #[test]
    fn the_detail_pane_takes_focus_and_scrolls() {
        let mut p = Pane::new(120, 40);
        p.app.data.glance.needs_you.push(frames::long_ask());
        let at = |p: &Pane| (p.app.row, p.app.detail);
        // Tab: asks, checks, campaigns, then the detail, then round; shift-tab the other way.
        p.keys("\t\t");
        assert_eq!(at(&p), (6, false));
        p.keys("\t");
        assert_eq!(at(&p), (6, true));
        p.keys("\t");
        assert_eq!(at(&p), (0, false));
        p.code(KeyCode::BackTab);
        assert_eq!(at(&p), (0, true));
        p.code(KeyCode::BackTab);
        assert_eq!(at(&p), (6, false));
        // h, esc and the left arrow return to the row the detail came from.
        for back in [KeyCode::Char('h'), KeyCode::Esc, KeyCode::Left] {
            p.keys("\t");
            assert_eq!(at(&p), (6, true));
            p.code(back);
            assert_eq!(at(&p), (6, false), "{back:?}");
        }
        assert_eq!(p.app.screen, Screen::Glance);

        // The long ask, then a click in the detail focuses it.
        p.keys("gjj");
        assert!(p.text().contains("Release runbook"));
        p.click(80, 10);
        assert_eq!(at(&p), (2, true));
        p.keys("j");
        assert_eq!(p.app.scroll, 1);
        p.keys("G");
        let (h, total) = p.app.seen.borrow().page;
        assert!(total > h, "{h} {total}");
        assert_eq!(p.app.scroll, total - h);
        let screen = p.text();
        assert!(
            screen.contains("12. Close the release")
                && screen.contains(&format!("{}-{total}/{total}", total - h + 1)),
            "{screen}"
        );
        // It stops at its ends: the last line at the bottom, never past it.
        p.keys("j");
        p.code(KeyCode::PageDown);
        assert_eq!(p.app.scroll, total - h);
        p.keys("g");
        p.code(KeyCode::Up);
        assert_eq!(p.app.scroll, 0);
        p.code(KeyCode::PageDown);
        assert_eq!(p.app.scroll, h.min(total - h));
        p.code(KeyCode::PageUp);
        assert_eq!((p.app.scroll, p.app.row), (0, 2));

        // Moving the cursor starts the next row's detail at its top: by key, by click.
        p.keys("G");
        p.keys("\x1bj");
        assert_eq!((p.app.row, p.app.scroll), (3, 0));
        p.keys("k");
        p.app.scroll = 4;
        p.click_on("ios-widgets");
        assert_eq!((p.app.row, p.app.scroll, p.app.detail), (1, 0, false));
        // and by a refresh that takes the row away; one that keeps it keeps the place.
        p.keys("j");
        p.app.scroll = 4;
        p.app.snapshot(p.app.data.glance.clone());
        assert_eq!((p.app.row, p.app.scroll), (2, 4));
        let mut next = p.app.data.glance.clone();
        next.needs_you.pop();
        p.app.snapshot(next);
        assert_eq!(p.app.scroll, 0);
    }

    #[test]
    fn the_wheel_scrolls_the_pane_under_it() {
        let mut p = Pane::new(120, 40);
        p.app.data.glance.needs_you.push(frames::long_ask());
        p.keys("jj");
        // Over the detail: it takes focus and scrolls; the cursor stays.
        p.mouse(MouseEventKind::ScrollDown, 80, 10);
        p.mouse(MouseEventKind::ScrollDown, 80, 10);
        assert_eq!((p.app.row, p.app.scroll, p.app.detail), (2, 2, true));
        p.mouse(MouseEventKind::ScrollUp, 80, 10);
        assert_eq!(p.app.scroll, 1);
        // Over the list: the cursor moves, and the detail starts at the top.
        p.mouse(MouseEventKind::ScrollDown, 5, 5);
        assert_eq!((p.app.row, p.app.scroll, p.app.detail), (3, 0, false));
    }

    #[test]
    fn narrow_there_is_no_detail_to_focus() {
        let mut p = Pane::new(46, 30);
        p.app.detail = true;
        // A focus left from a wide terminal does not take the keys of a narrow one.
        p.keys("j");
        assert_eq!((p.app.row, p.app.scroll), (1, 0));
        p.keys("\t\t\t");
        assert_eq!(p.app.row, 0);
    }

    /// Under 100 columns there is no detail pane; an ask's full text is read in the answer
    /// dialog, which scrolls it.
    #[test]
    fn narrow_the_answer_dialog_scrolls_a_long_ask_to_its_end() {
        let mut p = Pane::new(46, 30);
        p.app.data.glance.needs_you.push(frames::long_ask());
        p.keys("jja");
        let top = p.text();
        assert!(
            top.contains("Release runbook") && !top.contains("12. Close") && top.contains("/"),
            "{top}"
        );
        for _ in 0..10 {
            p.code(KeyCode::PageDown);
        }
        let (shown, total) = p.app.seen.borrow().page;
        let bottom = p.text();
        assert!(
            bottom.contains("12. Close the release")
                && bottom.contains(&format!("{}-{total}/{total}", total - shown + 1)),
            "{bottom}"
        );
    }

    #[test]
    fn pasted_text_is_never_keys() {
        let mut p = Pane::new(46, 30);
        p.keys("\t\t");
        let row = p.app.row;
        // On a campaign row, a pasted `py` would park it if it were keys.
        paste(&mut p.app, "py");
        assert!(p.app.pending.is_none() && p.app.row == row && p.app.screen == Screen::Glance);
        p.keys("/");
        paste(&mut p.app, "docs\n");
        assert_eq!((p.app.filter.as_str(), p.app.typing), ("docs", true));
        p.keys("\x1bga\t");
        let before = p.app.text.clone();
        paste(&mut p.app, ", and\nthen ship");
        assert_eq!(p.app.text, format!("{before}, and then ship"));
        assert_eq!(p.app.screen, Screen::Answer);
        // Not editing: nothing is taken, least of all the confirm step's `y`.
        p.keys("\t\n");
        paste(&mut p.app, "y");
        assert_eq!(p.app.screen, Screen::Confirm);
    }

    #[test]
    fn no_hint_names_a_key_that_does_nothing() {
        let mut p = Pane::new(120, 40);
        p.keys("\t\t");
        let foot = |p: &mut Pane| p.text().lines().last().unwrap().to_string();
        assert!(!foot(&mut p).contains("report"), "{}", foot(&mut p));
        assert_eq!(p.keys("o"), Effect::None);
        assert!(p.status().contains("l opens the campaign"));
        let root = p.app.data.campaign.root.id;
        p.app.row = glance::ids(&p.app.data.glance)
            .iter()
            .position(|r| *r == glance::RowId::Campaign(root))
            .unwrap();
        p.keys("lo");
        assert_eq!(p.app.screen, Screen::Pager);
        assert!(!foot(&mut p).contains("search") && foot(&mut p).contains("space"));
    }

    #[test]
    fn the_cursor_is_always_on_a_drawn_row() {
        for height in 8..=40 {
            let mut p = Pane::new(46, height);
            for keys in ["", "G", "\t\t", "Gk"] {
                p.keys(keys);
                let screen = p.text();
                assert!(
                    screen.lines().any(|l| l.starts_with('▌')),
                    "{height} rows after {keys:?}:\n{screen}"
                );
            }
        }
        // Under 8 rows there is the pill and nothing to move on.
        let mut p = Pane::new(46, 7);
        assert_eq!(p.text().lines().filter(|l| !l.is_empty()).count(), 1);
    }

    #[test]
    fn a_campaign_is_read_then_shown_and_follows_the_glance() {
        let mut p = Pane::new(120, 40);
        let sample: crate::model::Data =
            serde_json::from_str(include_str!("../tests/p1b-sample.json")).unwrap();
        p.app.data = sample.clone();
        p.keys("j");
        // `l` asks for the read and shows nothing yet.
        let job = Job::Campaign {
            root: 1,
            name: "demo-checkout".into(),
            open: true,
        };
        p.app.data.campaign = crate::model::Campaign::default();
        p.text();
        assert_eq!(
            press(&mut p.app, KeyCode::Char('l')),
            Effect::Run(job.clone())
        );
        assert_eq!(
            (p.app.screen, p.status()),
            (Screen::Glance, "reading demo-checkout")
        );
        assert!(follow(&p.app).is_none());
        done(
            &mut p.app,
            &job,
            Ok(Done::Campaign(Box::new(sample.campaign.clone()))),
        );
        assert_eq!(p.app.screen, Screen::Campaign);
        let screen = p.text();
        assert!(
            screen.contains("demo-worker")
                && screen.contains("#123")
                && screen.contains("Ship the sample."),
            "{screen}"
        );
        // Each new snapshot re-reads the campaign on screen; the cursor stays.
        p.keys("j");
        let again = follow(&p.app).expect("a refresh");
        assert_eq!(
            again,
            Job::Campaign {
                root: 1,
                name: "demo-checkout".into(),
                open: false
            }
        );
        let mut newer = sample.campaign.clone();
        newer.root.next = "Something newer".into();
        done(&mut p.app, &again, Ok(Done::Campaign(Box::new(newer))));
        assert_eq!(
            (p.app.lane, p.app.data.campaign.root.next.as_str()),
            (1, "Something newer")
        );
        // `o` on a lane with no report row says so; in Docs it asks for the document by
        // id, and the raw text opens in the pager.
        assert_eq!(p.keys("o"), Effect::None);
        assert_eq!(p.status(), "no report recorded for this lane");
        p.keys("3j");
        p.text();
        let Effect::Run(read) = press(&mut p.app, KeyCode::Char('o')) else {
            panic!("a read")
        };
        assert!(
            matches!(&read, Job::Doc { row } if row.kind == "brief" && row.id == 11),
            "{read:?}"
        );
        done(
            &mut p.app,
            &read,
            Ok(Done::Doc("# Report\n\nAll good.".into())),
        );
        assert_eq!(p.app.screen, Screen::Pager);
        assert!(p.text().contains("All good."));
        // A refresh that lands after the owner has left does not reopen anything.
        p.keys("qq");
        done(
            &mut p.app,
            &again,
            Ok(Done::Campaign(Box::new(sample.campaign.clone()))),
        );
        assert_eq!(p.app.screen, Screen::Glance);
        // A failed read says so in taskr's words.
        done(&mut p.app, &job, Err("unknown command campaign".into()));
        assert_eq!(
            p.status(),
            "! read demo-checkout failed: unknown command campaign"
        );
    }

    #[test]
    fn a_recorded_answer_is_not_called_failed() {
        let mut p = Pane::new(46, 30);
        let id = p.app.data.glance.needs_you[0].ask_id;
        let Effect::Run(job) = p.keys("a\ny") else {
            panic!("an answer")
        };
        done(
            &mut p.app,
            &job,
            Ok(Done::Note("not delivered to its pane: no pane".into())),
        );
        assert_eq!(
            p.status(),
            format!("✓ answered {id} · not delivered to its pane: no pane")
        );
        // It is recorded, so it cannot be sent again from here.
        p.keys("a");
        assert_eq!(p.app.screen, Screen::Glance);
        assert!(p.status().contains("already answered"));
        // A refusal is a failure, and the ask can be tried again.
        done(&mut p.app, &job, Err("server unreachable".into()));
        assert_eq!(
            p.status(),
            format!("! answer {id} failed: server unreachable")
        );
        p.keys("a");
        assert_eq!(p.app.screen, Screen::Answer);
    }

    #[test]
    fn the_loop_redraws_only_when_the_fetch_looks_different() {
        let mut fetch = crate::Fetch {
            loaded: true,
            age_ms: 2000,
            ..Default::default()
        };
        let face = fetch.face();
        fetch.age_ms = 2900;
        assert_eq!(fetch.face(), face, "the same second on screen");
        fetch.age_ms = 3000;
        assert_ne!(fetch.face(), face);
        let face = fetch.face();
        fetch.in_flight = true;
        assert_ne!(fetch.face(), face);
    }

    #[test]
    fn the_footer_says_whether_the_view_is_pushed_to_or_polling() {
        let mut p = Pane::new(100, 30);
        let last = |p: &mut Pane| {
            p.text()
                .lines()
                .last()
                .unwrap_or_default()
                .trim_end()
                .to_string()
        };
        assert!(!last(&mut p).ends_with("live") && !last(&mut p).ends_with("polling"));
        let face = p.app.fetch.face();
        p.app.fetch.live = Some(false);
        assert!(last(&mut p).ends_with(" polling"), "{}", last(&mut p));
        assert_ne!(p.app.fetch.face(), face, "a change of mode is drawn");
        p.app.fetch.live = Some(true);
        assert!(last(&mut p).ends_with(" live"), "{}", last(&mut p));
        // Pushed data does not go amber between safety polls; polled data does.
        p.app.fetch.loaded = true;
        p.app.fetch.age_ms = 40_000;
        assert!(last(&mut p).contains("? help"));
        // Narrow: hints give way, the mode and `? help` stay.
        let mut p = Pane::new(46, 30);
        p.app.fetch.live = Some(false);
        let line = last(&mut p);
        assert!(
            line.ends_with(" polling") && line.contains("? help"),
            "{line}"
        );
    }

    #[test]
    fn s_opens_the_slotr_view_and_its_rows_go_where_they_say() {
        let mut p = Pane::new(80, 24);
        p.keys("s");
        assert_eq!((p.app.screen, p.app.row), (Screen::Slotr, 0));
        assert!(p.text().contains("reading slotr"));
        // The loop reads slotr on its own cadence; the demo serves the fixture.
        demo(&mut p.app, Job::Slotr);
        let screen = p.text();
        assert!(
            screen.contains("HEAVY") && screen.contains("RUNTIME"),
            "{screen}"
        );
        // The heavy holder, on the hub, as every hub row: Enter focuses, y copies.
        let pane = "wF2:p3".to_string();
        assert_eq!(p.keys("\n"), Effect::Run(Job::Focus { pane }));
        assert_eq!(p.keys("y"), Effect::Copy("herdr agent focus wF2:p3".into()));
        // The heavy waiter has no ledger task: its campaign is found by name.
        p.keys("j");
        p.keys("l");
        assert!(p.status().contains("search-index"), "{}", p.status());
        // The last waiter has neither a task nor a pane nor a known campaign.
        p.keys("G");
        assert_eq!(p.app.row, 5);
        p.keys("l");
        assert_eq!(p.status(), "no campaign for this row");
        p.keys("\n");
        assert_eq!(p.status(), "no live pane for this row");
        // A click selects; a new glance does not move the slotr cursor.
        p.click_on("auth-rotation");
        assert_eq!(p.app.row, 4);
        p.app.snapshot(p.app.data.glance.clone());
        assert_eq!(p.app.row, 4);
        // The runtime holder's root is the campaign the demo holds: l opens it.
        p.keys("kk");
        p.keys("l");
        assert_eq!(p.app.screen, Screen::Campaign);
        // s from the campaign, and s again closes back to it.
        p.keys("s");
        assert_eq!(p.app.screen, Screen::Slotr);
        p.keys("s");
        assert_eq!(p.app.screen, Screen::Campaign);
        p.keys("hhh");
        assert_eq!(p.app.screen, Screen::Glance);
    }

    #[test]
    fn s_is_text_in_a_filter_and_in_an_answer() {
        let mut p = Pane::new(80, 24);
        p.keys("/s");
        assert_eq!((p.app.screen, p.app.filter.as_str()), (Screen::Glance, "s"));
        p.keys("\x1b");
        p.keys("a");
        assert_eq!(p.app.screen, Screen::Answer);
        p.app.editing = true;
        p.app.text.clear();
        p.keys("slots");
        assert_eq!(
            (p.app.screen, p.app.text.as_str()),
            (Screen::Answer, "slots")
        );
    }

    #[test]
    fn the_slotr_view_keeps_its_own_error_line() {
        let mut p = Pane::new(80, 24);
        p.keys("s");
        let error = "slotr: no systemd user bus here".to_string();
        let mut gone = p.app.data.slotr.clone();
        (gone.available, gone.error) = (false, error.clone());
        done(&mut p.app, &Job::Slotr, Ok(Done::Slotr(Box::new(gone))));
        assert_eq!(p.app.slotr.error.as_deref(), Some(error.as_str()));
        assert!(p.app.fetch.error.is_none() && p.app.status.is_none());
        assert!(p.text().contains("slotr unavailable: slotr: no systemd"));
        // The last good pools stay, and come back with the next good read.
        demo(&mut p.app, Job::Slotr);
        done(
            &mut p.app,
            &Job::Slotr,
            Err("taskr did not answer in 10s".into()),
        );
        assert!(p.app.slotr.loaded && p.app.data.slotr.pools.len() == 2);
        assert!(p.text().contains("HEAVY"));
        demo(&mut p.app, Job::Slotr);
        assert!(p.app.slotr.error.is_none());
    }
}
