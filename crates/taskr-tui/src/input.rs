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
    App, Hit, Outcome, Screen, Sent,
    actions::{self, Done, Job},
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
    SaveConfig,
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
        Screen::Confirm => confirm(app, key.code, ctrl),
        Screen::Picker => pick(app, key.code),
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

/// Puts the cursor of a screen of rows on `row`.
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
    if app.screen.rows() {
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

/// Scrolls the Asks screen's full ask, whose cursor stays on its row.
fn text_by(app: &mut App, by: isize) {
    let (height, total) = app.seen.borrow().page;
    app.scroll = app
        .scroll
        .saturating_add_signed(by)
        .min(total.saturating_sub(height));
}

fn scroll(app: &mut App, by: isize) -> Effect {
    travel(app, by);
    Effect::None
}

/// A new filter starts a fresh walk of the list, from the selected row when it still
/// matches.
fn reset(app: &mut App, before: Option<glance::RowId>) {
    (app.row, app.lane, app.scroll) = (0, 0, 0);
    if before.is_some() {
        app.restore_row(before);
    }
}

/// The selected row of a screen of rows, before its filter changes.
fn selected_id(app: &App) -> Option<glance::RowId> {
    app.screen
        .rows()
        .then(|| glance::ids(&app.view()).get(app.row).cloned())
        .flatten()
}

/// Typing in the filter: Enter keeps it, Esc clears it.
fn filter(app: &mut App, code: KeyCode) -> Effect {
    let before = selected_id(app);
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
    reset(app, before);
    Effect::None
}

/// The keys every screen but the answer dialog shares, then the screen's own.
fn view(app: &mut App, code: KeyCode, ctrl: bool) -> Effect {
    let (page, full) = {
        let seen = app.seen.borrow();
        (seen.page.0, seen.full)
    };
    let detail = app.screen == Screen::Detail;
    // The Asks screen's full ask scrolls on its own; `j k` keep moving between asks.
    let text = app.screen == Screen::Asks && full;
    let lists = app.screen.rows()
        || matches!(app.screen, Screen::AllCampaigns | Screen::Slotr)
        || (app.screen == Screen::Campaign && app.pane == 0);
    let half = if lists { 5 } else { (page / 2).max(1) } as isize;
    match code {
        // `q` closes before it quits.
        KeyCode::Char('q') if !app.close() => return Effect::Quit,
        KeyCode::Char('q') => {}
        // Back from Detail to the row it shows.
        KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left if detail => app.uninspect(),
        // Esc clears a filter before it closes anything.
        KeyCode::Esc if !app.filter.is_empty() => {
            let before = selected_id(app);
            app.filter.clear();
            reset(app, before);
        }
        KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left => {
            if !app.close() && !app.filter.is_empty() {
                let before = selected_id(app);
                app.filter.clear();
                reset(app, before);
            }
        }
        // The top-level screens, from any of them or from an overlay, which closes first.
        // Typing, the answer dialog and a pending `y` never reach here.
        KeyCode::Char('D') => app.switch(Screen::Glance),
        KeyCode::Char('A') => app.switch(Screen::Asks),
        KeyCode::Char('F') => app.switch(Screen::Fleet),
        KeyCode::Char('I') => app.inspect(),
        KeyCode::Char(c @ ('[' | ']')) => {
            let at = app.glance_view().screen.peer().unwrap_or(0);
            let by = if c == ']' { 1 } else { Screen::PEERS.len() - 1 };
            app.switch(Screen::PEERS[(at + by) % Screen::PEERS.len()]);
        }
        KeyCode::Char('?') if app.screen == Screen::Help => {
            app.close();
        }
        KeyCode::Char('?') => app.go(Screen::Help),
        // From a campaign opened out of slotr, `s` goes back rather than stacking another.
        KeyCode::Char('s')
            if app.screen == Screen::Slotr
                || (app.screen == Screen::Campaign && app.under() == Screen::Slotr) =>
        {
            app.close();
        }
        KeyCode::Char('s') if app.screen.peer().is_some() || app.screen == Screen::Campaign => {
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
        // No ask is selected on these: `a` lists the open ones to choose from.
        KeyCode::Char('a')
            if matches!(
                app.screen,
                Screen::Help | Screen::Pager | Screen::AllCampaigns | Screen::Slotr
            ) =>
        {
            app.picker();
        }
        KeyCode::Char('/')
            if matches!(
                app.screen,
                Screen::Glance
                    | Screen::Asks
                    | Screen::Fleet
                    | Screen::Campaign
                    | Screen::AllCampaigns
            ) =>
        {
            app.typing = true;
        }
        KeyCode::Char('j') | KeyCode::Down => travel(app, 1),
        KeyCode::Char('k') | KeyCode::Up => travel(app, -1),
        KeyCode::Char('g') | KeyCode::Home => travel(app, isize::MIN),
        KeyCode::Char('G') | KeyCode::End => travel(app, isize::MAX),
        KeyCode::Char('d') if ctrl && text => text_by(app, (page / 2).max(1) as isize),
        KeyCode::Char('u') if ctrl && text => text_by(app, -((page / 2).max(1) as isize)),
        KeyCode::PageDown if text => text_by(app, page as isize),
        KeyCode::PageUp if text => text_by(app, -(page as isize)),
        KeyCode::Char('d') if ctrl => travel(app, half),
        KeyCode::Char('u') if ctrl => travel(app, -half),
        KeyCode::Char(' ') if app.screen == Screen::Pager => travel(app, page as isize),
        KeyCode::PageDown if app.screen == Screen::Pager || detail => travel(app, page as isize),
        KeyCode::PageUp if app.screen == Screen::Pager || detail => travel(app, -(page as isize)),
        _ => {
            return match app.screen {
                Screen::Glance | Screen::Asks | Screen::Fleet | Screen::Detail => {
                    glance_key(app, code)
                }
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
    // The latest deliberate open wins; an earlier one's reply is dropped when it lands.
    app.scope.generation += 1;
    app.scope.want = Some((root, app.screen, app.back.len()));
    Effect::Run(Job::Campaign {
        root,
        name: name.to_string(),
        open: !loaded,
        seq: app.scope.generation,
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
        seq: app.scope.generation,
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
            // Nothing ran, so nothing is in flight.
            app.outcomes.retain(|s| s.job != job);
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
        let before = selected_id(app);
        app.filter.push_str(line.trim());
        reset(app, before);
    } else if app.screen == Screen::Answer && app.editing {
        app.edit(|text| text.push_str(&line));
    }
}

fn start_answer(app: &mut App, need: Option<crate::model::Need>) -> Effect {
    match need {
        None => {
            app.picker();
            Effect::None
        }
        Some(a) => {
            let id = a.ask_id;
            let kept = app
                .outcome(id)
                .map(|s| (s.state.clone(), s.job.clone(), s.ask.clone()));
            // A refused answer is kept to read in full, as it was sent; nothing sends it.
            if let Some((Outcome::Refused(_), Job::Answer { text, .. }, sent)) = kept.clone() {
                app.answer(sent);
                (app.text, app.editing, app.screen) = (text, false, Screen::Confirm);
                return Effect::None;
            }
            if let Some(line) = held(app, id) {
                return say(app, line);
            }
            match kept {
                // Still open after an unknown outcome: the same answer to the ask as it was
                // sent, to resend unchanged.
                Some((Outcome::Open, Job::Answer { text, .. }, sent)) => {
                    app.answer(sent);
                    (app.text, app.editing) = (text, false);
                    (app.screen, app.resend) = (Screen::Confirm, true);
                    Effect::None
                }
                _ => {
                    app.answer(a);
                    Effect::None
                }
            }
        }
    }
}

/// Why an ask takes no new answer from here: one is on its way, recorded, refused or
/// unknown. None when a new draft (or, still open after an unknown outcome, the resend) may
/// open.
fn held(app: &App, id: i64) -> Option<String> {
    Some(match &app.outcome(id)?.state {
        Outcome::Sending => format!("ask {id}: the answer is on its way"),
        Outcome::Recorded { answer_id, .. } => {
            format!("ask {id} is already answered from here (event {answer_id})")
        }
        Outcome::Refused(reason) => format!("! ask {id}: not recorded: {reason}"),
        Outcome::Unknown { .. } => {
            format!("? ask {id}: answer outcome unknown; checking the ledger")
        }
        Outcome::Open | Outcome::Gone => return None,
    })
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
    // Key 0 is the tenth panel, Issues; uppercase H the eleventh, Health. Lowercase h is
    // back, on every screen.
    // The Dashboard's alone: Asks and Fleet list their rows whatever the panels show.
    let panel = match code {
        _ if app.screen != Screen::Glance => None,
        KeyCode::Char(n @ '0'..='9') => Some((n as usize + 9 - '0' as usize) % 10),
        KeyCode::Char('H') => Some(10),
        _ => None,
    };
    if let Some(index) = panel {
        let before = glance::ids(&app.view()).get(app.row).cloned();
        app.config.panels[index] = !app.config.panels[index];
        app.restore_row(before);
        return Effect::SaveConfig;
    }
    let view = app.view();
    let drawn = if app.screen == Screen::Detail {
        view.row < glance::ids(&view).len()
    } else {
        app.seen
            .borrow()
            .hits
            .iter()
            .any(|(_, hit)| *hit == Hit::Row(app.row))
    };
    // `a` answers the selected ask; on any other row, or none, it opens the picker. Not
    // over a pane too small to draw anything but the pill.
    if code == KeyCode::Char('a') {
        if app.seen.borrow().hits.is_empty() {
            return Effect::None;
        }
        return match glance::selected(&view) {
            Selected::Ask(a) if drawn => start_answer(app, Some(a.clone())),
            _ => {
                app.picker();
                Effect::None
            }
        };
    }
    // On the Asks screen ⏎ answers the ask drawn in full; a narrow list of several reads the
    // selected one in full first.
    if code == KeyCode::Enter && app.screen == Screen::Asks && drawn {
        if !app.seen.borrow().full {
            app.inspect();
            return Effect::None;
        }
        if let Selected::Ask(a) = glance::selected(&view) {
            return start_answer(app, Some(a.clone()));
        }
    }
    let acts = matches!(
        code,
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('p' | 'l' | 'o' | 'y')
    );
    // Detail acts on its own item, found by identity; a list on a drawn row only.
    if acts && !drawn {
        if app.screen == Screen::Detail {
            return say(app, "nothing here to act on: esc returns");
        }
        return Effect::None;
    }
    let g = &view.data.glance;
    let (host, pane, root, name) = match glance::selected(&view) {
        Selected::Ask(a) => (&a.host, &a.pane_id, a.root_id, &a.campaign),
        Selected::Check(a) => (&a.host, &a.pane_id, a.root_id, &a.campaign),
        Selected::Campaign(c) => (&c.host, &c.pane_id, c.id, &c.name),
        Selected::Pr(_)
        | Selected::Author(_)
        | Selected::Run(_)
        | Selected::Assignee(_)
        | Selected::Health
            if matches!(code, KeyCode::Tab | KeyCode::BackTab | KeyCode::Char('c')) =>
        {
            (&g.now, &g.now, 0, &g.now)
        }
        Selected::Pr(_)
        | Selected::Author(_)
        | Selected::Run(_)
        | Selected::Assignee(_)
        | Selected::Health => {
            return Effect::None;
        }
        // Decisions, Fleet and the decision log only inform: `o` reads their campaign,
        // and nothing else here focuses, answers, parks, copies or writes.
        Selected::Decision(_) | Selected::Logged(_) | Selected::Fleet(_)
            if matches!(code, KeyCode::Tab | KeyCode::BackTab | KeyCode::Char('c')) =>
        {
            (&g.now, &g.now, 0, &g.now)
        }
        Selected::Decision(_) | Selected::Logged(_) => {
            let c = &view.data.campaign.root;
            return info_key(app, code, c.id, &c.name);
        }
        Selected::Fleet(c) => return info_key(app, code, c.id, &c.name),
        // An empty card's stop: tab moves on, `c` lists every campaign, nothing else acts.
        Selected::Nothing
            if matches!(code, KeyCode::Tab | KeyCode::BackTab | KeyCode::Char('c')) =>
        {
            (&g.now, &g.now, 0, &g.now)
        }
        Selected::Nothing => return Effect::None,
    };
    match code {
        // The Dashboard's cards, then round again.
        KeyCode::Tab | KeyCode::BackTab if app.screen == Screen::Glance => {
            let starts = glance::sections(&view);
            let tab = code == KeyCode::Tab;
            let next = if tab {
                starts.iter().find(|&&s| s > app.row)
            } else {
                starts.iter().rev().find(|&&s| s < app.row)
            };
            let wrap = if tab { starts.first() } else { starts.last() };
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

/// A Decisions, Fleet or decision-log row: `o` opens (re-reads) its campaign, as `l` does
/// on a campaign row. The other row keys do nothing here.
fn info_key(app: &mut App, code: KeyCode, root: i64, name: &str) -> Effect {
    match code {
        KeyCode::Char('o') => open(app, root, name),
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('p' | 'l' | 'y') => say(
            app,
            "an informational row: o opens its campaign; nothing else acts here",
        ),
        _ => Effect::None,
    }
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
                .find(|a| a.root_id == root.id && held(app, a.ask_id).is_none());
            return match need {
                Some(a) => start_answer(app, Some(a.clone())),
                None => {
                    app.picker();
                    Effect::None
                }
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
    let (options, multi) = app.ask.as_ref().map_or((0, false), |a| {
        let p = ask::parse(a);
        (p.options.len(), p.multi)
    });
    // In a multi-select ask, space picks the option under the cursor and Enter sends the set.
    let on_option = multi && app.choice < options;
    match code {
        KeyCode::Esc => {
            app.close();
            return say(app, "cancelled, nothing was sent");
        }
        KeyCode::Enter if on_option && app.picked.is_empty() => {
            return say(app, "pick at least one: space picks an option");
        }
        KeyCode::Enter if app.text.trim().is_empty() => return say(app, "write an answer first"),
        KeyCode::Enter => (app.screen, app.scroll) = (Screen::Confirm, 0),
        KeyCode::Tab | KeyCode::BackTab => app.editing = !app.editing,
        KeyCode::Down => app.choose((app.choice + 1).min(options)),
        KeyCode::Up => app.choose(app.choice.saturating_sub(1)),
        KeyCode::Backspace if app.editing => app.edit(|text| {
            text.pop();
        }),
        // ponytail: the cursor stays at the end of the text; no editing mid-line.
        KeyCode::Char(c) if app.editing => app.edit(|text| text.push(c)),
        KeyCode::Char(' ') if on_option => app.toggle(),
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
fn confirm(app: &mut App, code: KeyCode, ctrl: bool) -> Effect {
    // A long answer scrolls, as the ask's text does in the editor.
    let half = (app.seen.borrow().page.0 / 2).max(1) as isize;
    match code {
        KeyCode::Char('d') if ctrl => return scroll(app, half),
        KeyCode::Char('u') if ctrl => return scroll(app, -half),
        KeyCode::PageDown => return scroll(app, 2 * half),
        KeyCode::PageUp => return scroll(app, -2 * half),
        _ => {}
    }
    match (code, app.ask.clone()) {
        // A refused answer is shown to read; the ledger's reason holds, so nothing sends it.
        (KeyCode::Char('y' | 'e'), Some(a)) if app.refused().is_some() => {
            let reason = app.refused().unwrap_or_default().to_string();
            say(
                app,
                format!(
                    "! ask {}: not recorded: {reason}; it is not sent again",
                    a.ask_id
                ),
            )
        }
        (KeyCode::Esc | KeyCode::Char('q'), Some(a)) if app.refused().is_some() => {
            app.close();
            say(
                app,
                format!(
                    "ask {}: the refused answer stays here; a shows it",
                    a.ask_id
                ),
            )
        }
        // A resend's first send is still unresolved: only the resend is known not sent.
        (KeyCode::Char('y'), Some(a)) if app.gone && app.resend => say(
            app,
            format!(
                "! ask {} is no longer open; no resend was sent, and whether the first answer was recorded is unknown",
                a.ask_id
            ),
        ),
        (KeyCode::Char('y'), Some(a)) if app.gone => say(
            app,
            format!(
                "! ask {} is no longer open; nothing was sent · esc closes, the draft stays until then",
                a.ask_id
            ),
        ),
        // A resend is the job that went unknown, key and all; anything else is new.
        (KeyCode::Char('y'), Some(a)) if app.resend => {
            let Some(i) = app
                .outcomes
                .iter()
                .rposition(|s| s.ask.ask_id == a.ask_id && s.state == Outcome::Open)
            else {
                return say(app, format!("ask {}: nothing to resend", a.ask_id));
            };
            app.close();
            app.resend = false;
            app.outcomes[i].state = Outcome::Sending;
            app.status = Some(format!("resending the answer to ask {}", a.ask_id));
            Effect::Run(app.outcomes[i].job.clone())
        }
        // A second `y` never sends twice: the first one's outcome holds the ask.
        (KeyCode::Char('y'), Some(a))
            if app
                .outcome(a.ask_id)
                .is_some_and(|s| s.state != Outcome::Gone) =>
        {
            let line = held(app, a.ask_id)
                .unwrap_or_else(|| format!("ask {}: a shows the answer to resend", a.ask_id));
            say(app, line)
        }
        (KeyCode::Char('y'), Some(a)) => {
            app.close();
            let job = Job::Answer {
                ask_id: a.ask_id,
                text: app.text.trim().to_string(),
                prompt: !a.asker_waiting,
                key: actions::request_key(a.ask_id),
            };
            app.outcomes.push(Sent {
                ask: a.clone(),
                job: job.clone(),
                state: Outcome::Sending,
            });
            app.status = Some(format!("sending the answer to ask {}", a.ask_id));
            Effect::Run(job)
        }
        (KeyCode::Char('e'), _) if app.resend => say(
            app,
            "a resend is the same answer, unchanged: y resends it, esc keeps it",
        ),
        (KeyCode::Char('e'), _) => {
            app.screen = Screen::Answer;
            Effect::None
        }
        (KeyCode::Esc | KeyCode::Char('q'), Some(a)) if app.resend => {
            app.close();
            app.resend = false;
            say(
                app,
                format!("ask {}: no resend was sent; the answer is kept", a.ask_id),
            )
        }
        (KeyCode::Esc | KeyCode::Char('q'), _) => {
            app.close();
            say(app, "cancelled, nothing was sent")
        }
        _ => Effect::None,
    }
}

/// The picker owns every key; `A` is its one way to another screen.
fn pick(app: &mut App, code: KeyCode) -> Effect {
    match code {
        KeyCode::Char('j') | KeyCode::Down => app.pick_by(1),
        KeyCode::Char('k') | KeyCode::Up => app.pick_by(-1),
        KeyCode::Char('g') | KeyCode::Home => app.pick_by(isize::MIN),
        KeyCode::Char('G') | KeyCode::End => app.pick_by(isize::MAX),
        KeyCode::Enter => {
            let Some(a) = app.picked().cloned() else {
                return say(app, "nothing needs you: esc closes");
            };
            // The editor takes the picker's place: esc from it returns to the same origin.
            app.close();
            return start_answer(app, Some(a));
        }
        // `A` shows an open ask; a kept answer's ask is no longer on the Asks screen.
        KeyCode::Char('A') if app.picked_kept() => {
            return say(app, "no longer open: ⏎ reads the kept answer");
        }
        KeyCode::Char('A') => app.pick_full(),
        KeyCode::Esc | KeyCode::Char('q') => {
            app.close();
        }
        _ => {}
    }
    Effect::None
}

/// The result of a job the loop ran.
pub fn done(app: &mut App, job: &Job, result: Result<Done, String>) {
    let showing =
        app.screen == Screen::Campaign || app.back.iter().any(|b| b.screen == Screen::Campaign);
    if let Job::Slotr = job {
        // The slotr view says its own trouble, on its own line; the last good pools stay.
        let s = &mut app.slotr;
        match result {
            Ok(Done::Slotr(slotr)) if slotr.available => {
                // The cursor the slotr view owns: its own row when the view is on
                // top, else the nearest saved row of a slotr screen under an
                // overlay. The cursor follows its run, from the queue into a
                // slot too; no slotr screen anywhere, no cursor moves.
                let own = app.screen == Screen::Slotr;
                let saved = if own {
                    None
                } else {
                    app.back
                        .iter_mut()
                        .rev()
                        .find(|b| b.screen == Screen::Slotr)
                };
                let was = if own {
                    Some(app.row)
                } else {
                    saved.as_ref().map(|b| b.row)
                };
                let seq = was
                    .and_then(|row| slotr::rows(&app.data.slotr).get(row).map(|r| r.enqueue_seq));
                app.data.slotr = *slotr;
                (s.loaded, s.error) = (true, None);
                if let Some(row) = was {
                    let rows = slotr::rows(&app.data.slotr);
                    let at = seq
                        .and_then(|seq| rows.iter().position(|r| r.enqueue_seq == seq))
                        .unwrap_or(row.min(rows.len().saturating_sub(1)));
                    if own {
                        app.row = at;
                    } else if let Some(back) = saved {
                        back.row = at;
                    }
                }
            }
            Ok(Done::Slotr(slotr)) if !slotr.error.is_empty() => s.error = Some(slotr.error),
            Ok(_) => s.error = Some("slotr unavailable".into()),
            Err(error) => s.error = Some(error),
        }
        return;
    }
    if let Job::Campaign {
        root,
        name,
        open,
        seq,
    } = job
    {
        return campaign_done(app, (*root, name.as_str(), *open, *seq), showing, result);
    }
    app.status = match (job, result) {
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
        // ask stays out of reach here, and the line says what is left to do. A refusal is
        // the ledger's own; anything else is unknown, and nothing repeats it by itself.
        (Job::Answer { ask_id, .. }, result) => {
            let (state, line) = match result {
                Ok(Done::Answered { answer_id, note }) => {
                    let line = format!("✓ answered {ask_id} as event {answer_id}");
                    let line = if note.is_empty() {
                        line
                    } else {
                        format!("{line} · {note}")
                    };
                    (Outcome::Recorded { answer_id, note }, line)
                }
                Ok(Done::Refused(reason)) => (
                    Outcome::Refused(reason.clone()),
                    format!("! not recorded: {reason}"),
                ),
                // No receipt either way. Taking the time now, when the result is read, is
                // later than the failure: it can only make a read too old to settle it.
                _ => (
                    Outcome::Unknown {
                        after: Instant::now(),
                    },
                    format!("? ask {ask_id}: answer outcome unknown; checking the ledger"),
                ),
            };
            if let Some(sent) = app.outcomes.iter_mut().rev().find(|s| s.job == *job) {
                sent.state = state;
            }
            Some(line)
        }
        (Job::Park { name, park, .. }, Ok(_)) => Some(format!(
            "✓ {} {name}",
            if *park { "parked" } else { "unparked" }
        )),
        (_, Ok(_)) => None,
        (job, Err(error)) => Some(format!("! {} failed: {error}", job.label())),
    };
}

/// A campaign read came back. Only the latest deliberate open, or a refresh asked for since,
/// counts; it must be the campaign asked for. An open shows only from the screen it was asked
/// on. What lands becomes the cached campaign panels 7 and 9 read, with its own age.
fn campaign_done(
    app: &mut App,
    (root, name, open, seq): (i64, &str, bool, u64),
    showing: bool,
    result: Result<Done, String>,
) {
    if seq < app.scope.generation {
        return;
    }
    // The glance row, wherever it waits, stays on its identity when the read lands.
    let before = {
        let view = app.glance_view();
        glance::ids(&view).get(view.row).cloned()
    };
    let wanted = app.scope.want.filter(|w| w.0 == root);
    if wanted.is_some() {
        app.scope.want = None;
    }
    let result = match result {
        Ok(Done::Campaign(c)) if c.root.id == root => Ok(c),
        Ok(Done::Campaign(c)) => Err(format!("the ledger sent campaign {}", c.root.id)),
        Ok(_) => Err("the ledger sent no campaign".into()),
        Err(error) => Err(error),
    };
    let scoped = app.scope.read_at.is_some() && app.data.campaign.root.id == root;
    let campaign = match result {
        Ok(c) => c,
        Err(error) => {
            if wanted.is_some() || scoped {
                app.scope.error = Some(format!("reading {name} failed: {error}"));
            }
            app.status = Some(format!("! read {name} failed: {error}"));
            return;
        }
    };
    app.status = None;
    let here =
        wanted.is_some_and(|(_, screen, depth)| (screen, depth) == (app.screen, app.back.len()));
    if open && !showing {
        if !here {
            app.status = Some(format!(
                "{name} was read after the view moved on; l opens it"
            ));
            return;
        }
        app.data.campaign = *campaign;
        app.go(Screen::Campaign);
        (app.pane, app.lane) = (0, 0);
    } else if app.data.campaign.root.id == root && (showing || wanted.is_some()) {
        // A refresh under the view: the cursor stays where it can.
        app.lane = app.lane.min(campaign.lanes.len());
        app.data.campaign = *campaign;
    } else {
        return;
    }
    if wanted.is_some() || scoped {
        app.scope.read_at = Some(app.observed_at);
        app.scope.error = None;
        app.restore_row(before);
        app.click = None;
    }
}

/// The key a footer hint stands for.
fn hint_key(keys: &str) -> Option<KeyCode> {
    Some(match keys {
        "⏎" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "shift-tab" => KeyCode::BackTab,
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
            // In the answer dialog the wheel scrolls the ask's text, never the choice.
            if matches!(app.screen, Screen::Answer | Screen::Confirm) {
                return scroll(app, if down { 1 } else { -1 });
            }
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
                Hit::Row(i) => {
                    if app.screen.rows() {
                        select(app, i);
                    } else {
                        app.row = i;
                    }
                }
                Hit::Lane(i) => {
                    pane(app, 0);
                    app.lane = i;
                }
                Hit::Ask(id) => app.pick = Some(id),
                // Chooses, and nothing more: only `y` on the review sends.
                Hit::Opt(i) => {
                    if app.screen == Screen::Answer {
                        app.choose(i);
                    }
                    return Effect::None;
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
            app.config.repos = vec!["example-org/app".into()];
            app.config.issues_repo = Some("example-org/app".into());
            app.config.health_repo = Some("example-org/app".into());
            app.config.panels[3..6].fill(true);
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
        // At 20 rows the card windows and the body scrolls: G reaches the last campaign,
        // drawn (until slice 2 the folded campaigns were out of reach).
        let mut short = Pane::new(46, 20);
        short.keys("G");
        assert_eq!(short.app.row, last);
        short.text();
        assert!(drawn(&short, last));
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
        // A snapshot that reorders the asks under the dialog cannot change which ask is
        // answered. (One that drops it refuses `y`: see the closed-elsewhere test.)
        let mut next = p.app.data.glance.clone();
        next.needs_you.reverse();
        p.app.snapshot(next);
        let text = p.app.text.clone();
        assert!(text.starts_with("A: "), "{text}");
        let (job, sent) = answered(p.keys("y"));
        assert_eq!(sent, (ask.ask_id, text, false));
        assert_eq!(p.app.screen, Screen::Glance);
        let receipt = Done::Answered {
            answer_id: 9001,
            note: String::new(),
        };
        done(&mut p.app, &job, Ok(receipt));
        assert!(
            p.text()
                .contains(&format!("✓ answered {} as event 9001", ask.ask_id))
        );
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
        assert_eq!(answered(p.keys("\ny")).1, (ask.ask_id, text, true));
        // It cannot be sent twice from here while it is on its way.
        p.keys("a");
        assert_eq!(p.app.screen, Screen::Glance);
        assert!(p.status().contains("on its way"), "{}", p.status());

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
        // On a campaign row there is no ask to answer: `a` lists the open ones.
        p.keys("Ga");
        assert_eq!(p.app.screen, Screen::Picker);
        p.keys("\x1b");
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
        let ids = glance::ids(&p.app.view());
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
        let ids = glance::ids(&p.app.view());
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
        assert!(
            before
                .lines()
                .find(|l| l.starts_with('▌'))
                .unwrap()
                .contains('⏎')
                && before.contains("⏎ go")
        );
        old_taskr(&mut p);
        let screen = p.text();
        // The ask has no pane: no mark on its row, no hint, and Enter says why.
        assert!(
            !screen
                .lines()
                .find(|l| l.starts_with('▌'))
                .unwrap()
                .contains('⏎'),
            "{screen}"
        );
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
        let lead = glance::ids(&p.app.view())
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

    /// The answer a key sent: the job, and its ask, text and prompt flag. Its request key
    /// is checked against taskr's rule and set aside.
    fn answered(effect: Effect) -> (Job, (i64, String, bool)) {
        let Effect::Run(job) = effect else {
            panic!("no answer sent")
        };
        let Job::Answer {
            ask_id,
            text,
            prompt,
            key,
        } = &job
        else {
            panic!("not an answer: {job:?}")
        };
        assert!(
            (8..=128).contains(&key.len())
                && key
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "{key}"
        );
        let sent = (*ask_id, text.clone(), *prompt);
        (job, sent)
    }

    /// The selected row's identity on the screen shown.
    fn at_row(p: &Pane) -> Option<glance::RowId> {
        glance::ids(&p.app.view()).get(p.app.row).cloned()
    }

    #[test]
    fn screen_keys_switch_and_each_screen_keeps_its_place() {
        let mut p = Pane::new(120, 40);
        p.keys("jj");
        let dash = at_row(&p);
        // Asks lists asks only, Fleet campaigns only; switching runs nothing.
        assert_eq!(p.keys("A"), Effect::None);
        assert_eq!(p.app.screen, Screen::Asks);
        let ids = glance::ids(&p.app.view());
        assert!(!ids.is_empty() && ids.iter().all(|r| matches!(r, glance::RowId::Ask(_))));
        p.keys("j");
        let ask = at_row(&p);
        assert_eq!(p.keys("F"), Effect::None);
        assert_eq!(p.app.screen, Screen::Fleet);
        let ids = glance::ids(&p.app.view());
        assert!(!ids.is_empty() && ids.iter().all(|r| matches!(r, glance::RowId::Campaign(_))));
        p.keys("/doc\n");
        let fleet = at_row(&p);
        assert!(
            matches!(fleet, Some(glance::RowId::Campaign(4296))),
            "{fleet:?}"
        );
        // A snapshot that reorders every list while they wait keeps each place on its item.
        let mut next = p.app.data.glance.clone();
        next.needs_you.reverse();
        next.attention.reverse();
        next.campaigns.reverse();
        p.app.snapshot(next);
        p.keys("D");
        assert_eq!((p.app.screen, at_row(&p)), (Screen::Glance, dash));
        assert!(p.app.filter.is_empty());
        p.keys("]");
        assert_eq!((p.app.screen, at_row(&p)), (Screen::Asks, ask));
        p.keys("]");
        assert_eq!((p.app.screen, at_row(&p)), (Screen::Fleet, fleet));
        assert_eq!(p.app.filter, "doc");
        p.keys("]");
        assert_eq!(p.app.screen, Screen::Detail);
        p.keys("]");
        assert_eq!(p.app.screen, Screen::Glance);
        p.keys("[");
        assert_eq!(p.app.screen, Screen::Detail);
        // A click on a tab is its key; the tab line names the counts.
        let tabs = p.text().lines().nth(1).unwrap().to_string();
        assert!(
            tabs.contains("A Asks 2") && tabs.contains("[I Detail]"),
            "{tabs}"
        );
        p.click_on("A Asks");
        assert_eq!(p.app.screen, Screen::Asks);
        // q quits from any top-level screen.
        assert_eq!(p.keys("q"), Effect::Quit);
    }

    #[test]
    fn screen_keys_are_text_while_typing_and_never_switch_behind_a_dialog() {
        let mut p = Pane::new(46, 30);
        // In the filter they are letters.
        p.keys("/DAFI[]");
        assert_eq!(
            (p.app.screen, p.app.filter.as_str()),
            (Screen::Glance, "DAFI[]")
        );
        p.keys("\x1b\x1b");
        // Choosing an answer: ignored, the dialog stays.
        p.keys("a");
        p.keys("DAFI[]");
        assert_eq!(p.app.screen, Screen::Answer);
        // Writing one: text, a first bracket included.
        for _ in 0..3 {
            p.code(KeyCode::Down);
        }
        assert_eq!((p.app.text.as_str(), p.app.editing), ("", true));
        p.keys("[D]AFI");
        assert_eq!(
            (p.app.screen, p.app.text.as_str()),
            (Screen::Answer, "[D]AFI")
        );
        // The confirm step ignores them.
        p.keys("\nDAFI[]");
        assert_eq!(p.app.screen, Screen::Confirm);
        p.keys("\x1b");
        assert_eq!(p.app.screen, Screen::Glance);
        // A pending park: the key cancels it and does nothing else.
        p.keys("\t\tp");
        assert!(p.app.pending.is_some());
        assert_eq!(p.keys("A"), Effect::None);
        assert!(p.app.pending.is_none());
        assert_eq!(p.app.screen, Screen::Glance);
    }

    #[test]
    fn screen_keys_close_overlays_first() {
        let mut p = Pane::new(120, 40);
        for (open, overlay) in [
            ("?", Screen::Help),
            ("c", Screen::AllCampaigns),
            ("s", Screen::Slotr),
        ] {
            for (key, screen) in [
                ("F", Screen::Fleet),
                ("A", Screen::Asks),
                ("D", Screen::Glance),
            ] {
                p.keys(open);
                assert_eq!(p.app.screen, overlay);
                p.keys(key);
                assert_eq!(p.app.screen, screen, "{open} {key}");
                assert!(p.app.back.is_empty(), "{open} {key}");
            }
        }
        // Back from the screen switched to reopens nothing.
        p.keys("h");
        assert_eq!(p.app.screen, Screen::Glance);
    }

    #[test]
    fn detail_shows_the_selected_item_and_returns_to_its_row() {
        // Short enough that the long ask scrolls at full width.
        let mut p = Pane::new(80, 24);
        p.app.data.glance.needs_you.push(frames::long_ask());
        p.keys("jj");
        let ask = at_row(&p);
        p.keys("I");
        assert_eq!(p.app.screen, Screen::Detail);
        let top = p.text();
        assert!(
            top.contains("from Dashboard · NEEDS YOU · row 3") && top.contains("Release runbook"),
            "{top}"
        );
        // The full width: nothing is drawn beside it, nothing on it can be clicked as a row.
        assert!(!top.contains("Glance"));
        assert!(
            p.app
                .seen
                .borrow()
                .hits
                .iter()
                .all(|(_, h)| matches!(h, Hit::Key(_)))
        );
        // It scrolls to its last line and stops there.
        p.keys("G");
        let (h, total) = p.app.seen.borrow().page;
        assert!(total > h, "{h} {total}");
        assert_eq!(p.app.scroll, total - h);
        let end = p.text();
        assert!(
            end.contains("12. Close the release")
                && end.contains(&format!("{}-{total}/{total}", total - h + 1)),
            "{end}"
        );
        p.keys("j");
        p.code(KeyCode::PageDown);
        assert_eq!(p.app.scroll, total - h);
        p.keys("g");
        assert_eq!(p.app.scroll, 0);
        p.mouse(MouseEventKind::ScrollDown, 60, 10);
        p.mouse(MouseEventKind::ScrollDown, 60, 10);
        assert_eq!(p.app.scroll, 2);
        // h, esc and the left arrow return to the row it came from; I opens it again at
        // its top.
        for back in [KeyCode::Char('h'), KeyCode::Esc, KeyCode::Left] {
            p.code(back);
            assert_eq!((p.app.screen, at_row(&p)), (Screen::Glance, ask.clone()));
            p.keys("I");
            assert_eq!((p.app.screen, p.app.scroll), (Screen::Detail, 0));
        }
        // From a filtered Fleet: back to its row under its filter.
        p.keys("F/doc\nI");
        let text = p.text();
        assert!(
            text.contains("from Fleet · CAMPAIGNS · row 1") && text.contains("docs-refresh"),
            "{text}"
        );
        p.keys("\x1b");
        assert_eq!(
            (p.app.screen, p.app.filter.as_str(), at_row(&p)),
            (Screen::Fleet, "doc", Some(glance::RowId::Campaign(4296)))
        );
        // Narrow, the long ask still reaches its last line.
        let mut p = Pane::new(46, 20);
        p.app.data.glance.needs_you.push(frames::long_ask());
        // As in the app, a frame is drawn between keys: G scrolls the page Detail drew.
        p.keys("jjI");
        p.text();
        p.keys("G");
        let text = p.text();
        // Its options and its campaign's activity follow the body: the end is the last page.
        let (h, total) = p.app.seen.borrow().page;
        assert_eq!(p.app.scroll, total - h);
        assert!(
            text.contains("ask 61812")
                && text.contains(&format!("{}-{total}/{total}", total - h + 1))
                && text.contains("B tomorrow morning"),
            "{text}"
        );
    }

    #[test]
    fn detail_never_retargets_when_its_item_leaves() {
        let mut p = Pane::new(46, 30);
        let ask = p.app.data.glance.needs_you[0].ask_id;
        let other = p.app.data.glance.needs_you[1].ask_id;
        p.keys("I");
        assert!(p.text().contains(&format!("ask {ask}")));
        // A snapshot that moves it keeps it.
        let mut next = p.app.data.glance.clone();
        next.needs_you.reverse();
        p.app.snapshot(next.clone());
        assert!(p.text().contains(&format!("ask {ask}")));
        // Answered elsewhere: Detail says so, and no key acts on the ask now in its place.
        next.needs_you.retain(|a| a.ask_id != ask);
        p.app.snapshot(next);
        let text = p.text();
        assert!(
            text.contains("No longer listed") && !text.contains(&format!("ask {other}")),
            "{text}"
        );
        for key in ["p", "l", "o", "y", "\n"] {
            assert_eq!(p.keys(key), Effect::None, "{key:?}");
            assert_eq!(p.app.screen, Screen::Detail);
            assert!(p.app.pending.is_none() && p.app.ask.is_none());
        }
        p.text();
        a_lists_asks(&mut p);
        assert!(!p.text().lines().last().unwrap().contains("answer"));
        p.keys("\x1b");
        assert_eq!(p.app.screen, Screen::Glance);
        // With nothing ever opened, Detail says how to fill it.
        let mut p = Pane::new(46, 30);
        p.keys("[");
        let text = p.text();
        assert_eq!(p.app.screen, Screen::Detail);
        assert!(text.contains("Select a row first"), "{text}");
    }

    #[test]
    fn detail_keeps_every_row_kind_whole_and_acts_on_its_own_item() {
        let mut p = Pane::new(120, 200);
        let now = 1_792_000_000;
        p.app.observed_at = now;
        p.app.mine = crate::prs::demo(p.app.observed_at);
        p.app.shipped = crate::github::demo_shipped(now);
        p.app.builds = crate::github::demo_builds(now);
        p.app.issues = crate::github::demo_issues(now);
        p.app.health = crate::health::sample(now, crate::health::CLEAN);
        p.app.config.panels = [true; 11];
        p.app.scope.read_at = Some(now - 180);
        let ids = glance::ids(&p.app.view());
        let mut kinds = vec![];
        for (i, id) in ids.iter().enumerate() {
            p.app.row = i;
            p.keys("I");
            let text = p.text();
            // What the row's old detail pane said, each with its source's age or sample.
            let (card, field) = match id {
                glance::RowId::Ask(n) => ("NEEDS YOU", format!("ask {n}")),
                glance::RowId::Check(..) => ("TO CHECK", "check · ".into()),
                glance::RowId::Campaign(n) => ("CAMPAIGNS", format!("#{n}")),
                glance::RowId::Pr(..) => ("MY PRS", "source: ".into()),
                glance::RowId::Author(_) => ("SHIPPED", "Observed cached sample".into()),
                glance::RowId::Run(..) => ("BUILD QUEUE", "Latest100-run sample".into()),
                glance::RowId::Decision(..) => ("DECISIONS", "cached read".into()),
                glance::RowId::Fleet(_) => ("FLEET", "as the glance reported them".into()),
                glance::RowId::Logged(..) => ("DECISION LOG", "recent log sample".into()),
                glance::RowId::Assignee(..) => ("ISSUES", "Current assignees".into()),
                glance::RowId::Health(..) => ("HEALTH", "Health · ".into()),
                // Every source here is full: no card is empty, so no inert stop is listed.
                glance::RowId::Card(slot) => panic!("inert stop for card {slot} with full sources"),
            };
            assert!(
                text.contains(&format!("from Dashboard · {card} · row"))
                    && text.contains(&field)
                    && !text.contains("No longer listed"),
                "{id:?}:\n{text}"
            );
            // Detail admits no GitHub read: no `r` is offered for what it cannot refresh.
            let foot = text.lines().last().unwrap_or_default().to_string();
            if matches!(
                id,
                glance::RowId::Pr(..) | glance::RowId::Author(_) | glance::RowId::Run(..)
            ) {
                assert!(!foot.contains("r refresh"), "{id:?}: {foot}");
                p.keys("h");
                let foot = p.text().lines().last().unwrap_or_default().to_string();
                assert!(foot.contains("r refresh"), "{id:?} row: {foot}");
                p.keys("I");
            }
            // A campaign's own lead and note are said once, not again under its activity.
            if matches!(id, glance::RowId::Campaign(_) | glance::RowId::Fleet(_)) {
                assert!(
                    text.contains("last 4 h") && !text.contains("│ last "),
                    "{text}"
                );
            }
            // A PR says the read it came from, as Shipped and Build do.
            if matches!(id, glance::RowId::Pr(..)) {
                assert!(
                    text.contains("source: 6 observed · demo-org/demo · 0s ago"),
                    "{text}"
                );
            }
            kinds.push(card);
            p.keys("h");
            assert_eq!((p.app.screen, p.app.row), (Screen::Glance, i));
        }
        // A failed read: the PR's Detail says stale, not current.
        p.app.mine.error = Some("GitHub unavailable");
        p.app.row = ids
            .iter()
            .position(|r| matches!(r, glance::RowId::Pr(..)))
            .unwrap();
        p.keys("I");
        let text = p.text();
        assert!(text.contains("source: stale 0s; unavailable"), "{text}");
        p.keys("h");
        p.app.mine.error = None;
        kinds.dedup();
        assert_eq!(kinds.len(), 11, "{kinds:?}");
        // On Detail the row keys act on its item as on its row: a to-check row opens its
        // campaign, an informational row has o only.
        p.app.row = ids
            .iter()
            .position(|r| matches!(r, glance::RowId::Campaign(_)))
            .unwrap();
        p.keys("I");
        let c = match glance::selected(&p.app.view()) {
            Selected::Campaign(c) => (c.id, c.name.clone()),
            _ => panic!("Detail is not on a campaign"),
        };
        p.keys("p");
        assert!(matches!(&p.app.pending, Some(Job::Park { root, .. }) if *root == c.0));
        p.keys("x");
        p.app.row = 0;
        p.keys("hgI");
        assert!(matches!(glance::selected(&p.app.view()), Selected::Ask(_)));
        p.keys("a");
        assert_eq!(p.app.screen, Screen::Answer);
        p.keys("\x1b");
        assert_eq!(p.app.screen, Screen::Detail);
    }

    #[test]
    fn hidden_asks_and_campaigns_are_reached_on_their_own_screens_only() {
        let mut p = Pane::new(46, 30);
        p.keys("13");
        let only = |p: &Pane, ask: bool| {
            glance::ids(&p.app.view()).iter().any(|r| {
                if ask {
                    matches!(r, glance::RowId::Ask(_))
                } else {
                    matches!(r, glance::RowId::Campaign(_))
                }
            })
        };
        assert!(!only(&p, true) && !only(&p, false));
        p.keys("A");
        assert_eq!(
            glance::ids(&p.app.view()).len(),
            p.app.data.glance.needs_you.len()
        );
        p.keys("a");
        assert_eq!(p.app.screen, Screen::Answer);
        p.keys("\x1b");
        assert_eq!(p.app.screen, Screen::Asks);
        // The panel keys are the Dashboard's.
        assert_eq!(p.keys("1"), Effect::None);
        assert!(!p.app.config.panels[0]);
        assert!(!p.text().contains("1 asks"));
        p.keys("F");
        assert!(only(&p, false));
        p.keys("p");
        assert!(matches!(p.app.pending, Some(Job::Park { .. })));
        p.keys("x");
        p.keys("D");
        assert!(!only(&p, true) && !only(&p, false));
    }

    #[test]
    fn sources_are_admitted_only_on_the_dashboard_with_nothing_over_it() {
        let mut p = Pane::new(120, 40);
        p.app.config.panels = [true; 11];
        let sources = [3, 4, 5, 9, 10];
        let all = |app: &App| sources.iter().all(|&i| app.admits(i));
        let none = |app: &App| sources.iter().all(|&i| !app.admits(i));
        assert!(all(&p.app));
        for (keys, screen) in [
            ("A", Screen::Asks),
            ("F", Screen::Fleet),
            ("I", Screen::Detail),
            ("D?", Screen::Help),
            ("Dc", Screen::AllCampaigns),
            ("Ds", Screen::Slotr),
            ("Dga", Screen::Answer),
            ("Dga\n", Screen::Confirm),
            // The picker parks every source, from any screen it opens on.
            ("DGa", Screen::Picker),
            ("Fa", Screen::Picker),
            ("Ica", Screen::Picker),
        ] {
            assert!(!matches!(p.keys(keys), Effect::Refresh | Effect::Run(_)));
            assert_eq!(p.app.screen, screen, "{keys}");
            assert!(none(&p.app), "{keys}");
            while p.app.close() {}
            p.app.switch(Screen::Glance);
            assert!(all(&p.app), "{keys}");
        }
        // A hidden card admits nothing, on the Dashboard too.
        p.keys("4");
        assert!(!p.app.admits(3) && p.app.admits(4));
        // A pending park question is a modal on the Dashboard: nothing reads under it,
        // and the key that cancels it gives the schedule back.
        let mut p = Pane::new(46, 30);
        let all = |app: &App| [3, 4, 5].iter().all(|&i| app.admits(i));
        let none = |app: &App| [3, 4, 5].iter().all(|&i| !app.admits(i));
        p.keys("\t\t");
        campaign(&p.app);
        assert!(all(&p.app));
        assert_eq!(p.keys("p"), Effect::None);
        assert!(p.app.pending.is_some() && p.app.screen == Screen::Glance);
        assert!(none(&p.app));
        assert_eq!(p.keys("j"), Effect::None);
        assert!(p.app.pending.is_none() && all(&p.app));
    }

    #[test]
    fn the_fleet_tab_and_the_campaigns_rule_count_the_same_rows() {
        let mut p = Pane::new(120, 40);
        let listed = p.app.data.glance.campaigns.len();
        assert!(p.app.data.glance.campaigns.iter().any(|c| c.parked));
        p.keys("D");
        let text = p.text();
        let rule = text.lines().find(|l| l.contains("CAMPAIGNS")).unwrap();
        assert!(
            text.contains(&format!("F Fleet {listed}"))
                && rule.trim_end().ends_with(&format!(" {listed}")),
            "D:\n{text}"
        );
        // Fleet's totals line counts the same listed campaigns, and says they are only those.
        p.keys("F");
        let text = p.text();
        assert!(
            text.contains(&format!("F Fleet {listed}"))
                && text.contains(&format!(
                    "in {listed} listed campaigns (listed only, not every host)"
                )),
            "F:\n{text}"
        );
    }

    #[test]
    fn tab_cycles_cards_and_never_stops_on_a_pane() {
        for width in [46, 120] {
            let mut p = Pane::new(width, 40);
            p.keys("\t");
            assert_eq!(p.app.row, 2);
            p.keys("\t");
            assert_eq!(p.app.row, 5);
            p.keys("\t");
            assert_eq!(p.app.row, 0);
            p.code(KeyCode::BackTab);
            assert_eq!((p.app.row, p.app.screen), (5, Screen::Glance));
            // The footer names no pane keys and the screens off the Dashboard no panels.
            let foot = |p: &mut Pane| p.text().lines().last().unwrap().to_string();
            assert!(!foot(&mut p).contains("list"));
            for key in ["A", "F"] {
                p.keys(key);
                let f = foot(&mut p);
                assert!(!f.contains("0-9") && !f.contains("health"), "{key}: {f}");
            }
            p.keys("D[");
            let f = foot(&mut p);
            assert!(
                f.contains("h back")
                    && !f.contains("answer")
                    && !f.contains("copy")
                    && !f.contains("filter")
                    && !f.contains("scroll"),
                "{f}"
            );
        }
    }

    /// An ask's full text is read in the answer dialog too, which scrolls it.
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
        p.app.row = glance::ids(&p.app.view())
            .iter()
            .position(|r| *r == glance::RowId::Campaign(root))
            .unwrap();
        p.keys("lo");
        assert_eq!(p.app.screen, Screen::Pager);
        assert!(!foot(&mut p).contains("search") && foot(&mut p).contains("space"));
    }

    #[test]
    fn panel_recovery_hidden_actions_and_pr_identity() {
        for height in [7, 8, 9, 20, 30] {
            let mut p = Pane::new(46, height);
            p.app.mine = crate::prs::demo(p.app.observed_at);
            p.app.config_warning = Some("TUI config invalid; using defaults".into());
            for n in '1'..='6' {
                assert_eq!(p.code(KeyCode::Char(n)), Effect::SaveConfig);
            }
            let text = p.text();
            assert!(text.contains("hidden asks"), "{height}: {text}");
            assert!(text.contains("1-9"), "recovery hint at {height}: {text}");
            // Tiny (under 20 rows) has no switch strip; the recovery line names the keys.
            if height >= 20 {
                assert!(
                    text.contains("panels 1- 2- 3- 4- 5- 6-"),
                    "all four switches at {height}: {text}"
                );
            }
            assert!(glance::ids(&p.app.view()).is_empty());
            for code in [KeyCode::Enter, KeyCode::Char('p')] {
                assert_eq!(p.code(code), Effect::None);
            }
            assert!(p.app.pending.is_none());
            assert!(p.app.ask.is_none());
            p.text();
            a_lists_asks(&mut p);
            assert_eq!(p.code(KeyCode::Char('4')), Effect::SaveConfig);
            assert!(matches!(glance::selected(&p.app.view()), Selected::Pr(_)));
            for code in [
                KeyCode::Enter,
                KeyCode::Char('p'),
                KeyCode::Char('l'),
                KeyCode::Char('y'),
            ] {
                assert_eq!(p.code(code), Effect::None);
            }
            p.text();
            a_lists_asks(&mut p);
            assert_eq!(p.code(KeyCode::Char('1')), Effect::SaveConfig);
            // Showing asks preserves the PR by repository and number instead of retargeting an ask.
            assert!(matches!(glance::selected(&p.app.view()), Selected::Pr(_)));
            p.keys("g");
            assert!(matches!(glance::selected(&p.app.view()), Selected::Ask(_)));
            if height >= 8 {
                assert!(p.text().lines().any(|l| l.starts_with('▌')));
            }
        }
        let mut p = Pane::new(46, 30);
        p.app.mine = crate::prs::demo(p.app.observed_at);
        p.app.row = glance::ids(&p.app.view())
            .iter()
            .position(|id| matches!(id, glance::RowId::Pr(_, 40)))
            .unwrap();
        let before = glance::ids(&p.app.view())[p.app.row].clone();
        let at40 = |m: &crate::prs::Mine| m.prs.iter().position(|p| p.number == 40);
        assert_eq!(at40(&p.app.mine), Some(4));
        // Passing #40 starts failing: the next cycle sorts it among the failing, after newer #42.
        let mut sample = p.app.mine.sample.clone();
        let f = sample.prs.iter_mut().find(|p| p.number == 40).unwrap();
        if let Some(Ok(c)) = f.read.as_mut().map(|f| &mut f.checks) {
            c.fail = 1;
        }
        let (tick, at) = (p.app.observed_tick, p.app.observed_at);
        p.app.mine.land(Ok(sample), tick, at);
        assert_eq!(at40(&p.app.mine), Some(1));
        p.app.restore_row(Some(before.clone()));
        assert_eq!(glance::ids(&p.app.view())[p.app.row], before);
        let mut snapshot = p.app.data.glance.clone();
        snapshot.needs_you.reverse();
        p.app.snapshot(snapshot);
        assert_eq!(glance::ids(&p.app.view())[p.app.row], before);
        p.keys("/no-matches");
        p.code(KeyCode::Enter);
        // Nothing matches: every enabled card is empty, so each is an inert stop that is drawn
        // and reached, and none answers, mutates or reads anything.
        let stops: Vec<_> = (0..4).map(glance::RowId::Card).collect();
        assert_eq!(glance::ids(&p.app.view()), stops);
        for row in 0..stops.len() {
            p.app.row = row;
            assert!(matches!(glance::selected(&p.app.view()), Selected::Nothing));
            assert_reachable(&mut p, false);
            for key in ['\n', 'p', 'l', 'y', 'o'] {
                assert_eq!(p.keys(&key.to_string()), Effect::None, "{row} {key:?}");
                assert!(
                    p.app.ask.is_none() && p.app.pending.is_none(),
                    "{row} {key:?}"
                );
                assert_eq!(p.app.row, row, "{row} {key:?}");
            }
            // `a` lists the open asks; esc gives back this stop and its filter.
            p.text();
            a_lists_asks(&mut p);
            assert_eq!((p.app.row, p.app.filter.as_str()), (row, "no-matches"));
        }
    }

    #[test]
    fn source_index_shift_clears_click_before_it_can_focus_another_campaign() {
        let mut p = Pane::new(120, 40);
        // The PRs come before the campaigns (band B over band C): one leaving shifts them.
        p.app.mine = crate::prs::demo(p.app.observed_at);
        let ids = glance::ids(&p.app.view());
        let i = ids
            .iter()
            .position(|r| matches!(r, glance::RowId::Campaign(_)))
            .unwrap();
        p.app.row = i;
        p.text();
        let area = p
            .app
            .seen
            .borrow()
            .hits
            .iter()
            .find(|(_, hit)| *hit == Hit::Row(i))
            .unwrap()
            .0;
        assert_eq!(p.click(area.x, area.y), Effect::None);
        assert!(p.app.click.is_some());
        p.app.scroll = 4;
        p.app.mine.prs.pop();
        p.app.restore_row(Some(ids[i].clone()));
        assert_eq!(p.app.row, i - 1);
        assert_eq!(glance::ids(&p.app.view())[p.app.row], ids[i]);
        assert_eq!(p.app.scroll, 0);
        assert!(p.app.click.is_none());
        p.text();
        let area = p
            .app
            .seen
            .borrow()
            .hits
            .iter()
            .find(|(_, hit)| *hit == Hit::Row(i))
            .unwrap()
            .0;
        assert_eq!(p.click(area.x, area.y), Effect::None);
        assert_eq!(p.app.row, i);
        assert!(p.app.pending.is_none() && p.app.ask.is_none());
        assert_eq!(p.app.screen, Screen::Glance);
    }

    #[test]
    fn shipped_expiry_is_a_controlled_view_transition_without_fetch() {
        let mut p = Pane::new(120, 40);
        let now = 1_792_000_000;
        p.app.observed_at = now;
        p.app.shipped = crate::github::demo_shipped(now);
        for s in &mut p.app.shipped.sources {
            s.merged.retain(|r| r.author == "alex");
            for r in &mut s.merged {
                r.merged = now - 86400;
            }
        }
        let campaign = glance::ids(&p.app.view())
            .into_iter()
            .find(|r| matches!(r, glance::RowId::Campaign(_)))
            .unwrap();
        p.app.row = glance::ids(&p.app.view())
            .iter()
            .position(|r| *r == campaign)
            .unwrap();
        assert!(p.text().contains("@alex · 2 PRs observed"));
        assert!(p.app.advance_time(now + 1, Instant::now()));
        assert_eq!(glance::ids(&p.app.view())[p.app.row], campaign);
        assert!(p.app.seen.borrow().hits.is_empty());
        assert!(!p.text().contains("@alex · 2 PRs observed"));
        assert!(!p.app.advance_time(now + 2, Instant::now()));

        p.app.observed_at = now;
        p.app.row = glance::ids(&p.app.view())
            .iter()
            .position(|r| matches!(r, glance::RowId::Author(_)))
            .unwrap();
        p.text();
        let rect = p
            .app
            .seen
            .borrow()
            .hits
            .iter()
            .find(|(_, hit)| *hit == Hit::Row(p.app.row))
            .unwrap()
            .0;
        p.app.click = Some((Instant::now(), Hit::Row(p.app.row)));
        p.app.scroll = 4;
        assert!(p.app.advance_time(now + 1, Instant::now()));
        assert!(p.app.click.is_none());
        assert_eq!(p.app.scroll, 0);
        for key in [
            KeyCode::Enter,
            KeyCode::Char('a'),
            KeyCode::Char('p'),
            KeyCode::Char('l'),
        ] {
            assert_eq!(press(&mut p.app, key), Effect::None);
        }
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(mouse(&mut p.app, click, Instant::now()), Effect::None);
        assert!(p.app.pending.is_none() && p.app.ask.is_none());
        assert_eq!(p.app.screen, Screen::Glance);
        assert!(!p.text().contains("@alex · 2 PRs observed"));
    }

    #[test]
    fn expiry_under_help_preserves_the_underlying_campaign_selection() {
        let mut p = Pane::new(120, 40);
        let now = 1_792_000_000;
        p.app.observed_at = now;
        p.app.shipped = crate::github::demo_shipped(now);
        for s in &mut p.app.shipped.sources {
            for r in &mut s.merged {
                r.merged = now - 86400;
            }
        }
        let id = glance::ids(&p.app.view())
            .into_iter()
            .find(|r| matches!(r, glance::RowId::Campaign(_)))
            .unwrap();
        p.app.row = glance::ids(&p.app.view())
            .iter()
            .position(|r| *r == id)
            .unwrap();
        p.keys("?");
        assert_eq!(p.app.screen, Screen::Help);
        assert!(p.app.advance_time(now + 1, Instant::now()));
        p.keys("\x1b");
        assert_eq!(p.app.screen, Screen::Glance);
        assert_eq!(glance::ids(&p.app.view())[p.app.row], id);
    }

    #[test]
    fn six_panel_rows_are_reachable_and_external_rows_never_act_on_ledger() {
        for height in [7, 8, 9, 12, 20, 30, 40] {
            let mut p = Pane::new(46, height);
            p.app.mine = crate::prs::demo(p.app.observed_at);
            p.app.shipped = crate::github::demo_shipped(p.app.observed_at);
            p.app.builds = crate::github::demo_builds(p.app.observed_at);
            let ids = glance::ids(&p.app.view());
            for (i, id) in ids.iter().enumerate() {
                p.app.row = i;
                let text = p.text();
                if height >= 8 {
                    assert!(
                        p.app
                            .seen
                            .borrow()
                            .hits
                            .iter()
                            .any(|(_, hit)| *hit == Hit::Row(i)),
                        "{height}/{id:?}: {text}"
                    );
                }
                if matches!(id, glance::RowId::Author(_) | glance::RowId::Run(..)) || height < 8 {
                    for code in [
                        KeyCode::Enter,
                        KeyCode::Char('p'),
                        KeyCode::Char('l'),
                        KeyCode::Char('y'),
                    ] {
                        assert_eq!(press(&mut p.app, code), Effect::None, "{height}/{id:?}");
                    }
                    assert!(p.app.ask.is_none() && p.app.pending.is_none());
                    a_lists_asks(&mut p);
                }
            }
            for n in '1'..='6' {
                p.code(KeyCode::Char(n));
            }
            assert!(glance::ids(&p.app.view()).is_empty());
            assert!(p.text().contains("1-9"));
            p.code(KeyCode::Char('5'));
            assert!(matches!(
                glance::selected(&p.app.view()),
                Selected::Author(_)
            ));
            p.code(KeyCode::Char('5'));
            p.code(KeyCode::Char('6'));
            assert!(matches!(glance::selected(&p.app.view()), Selected::Run(_)));
        }
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
        assert_eq!(p.keys("a"), Effect::None);
        assert_eq!(p.keys("\n"), Effect::None);
        assert!(p.app.ask.is_none());
    }

    #[test]
    fn dashboard_cursor_stays_drawn_with_prs_and_a_warning() {
        for height in 8..=40 {
            let mut p = Pane::new(46, height);
            p.app.mine = crate::prs::demo(p.app.observed_at);
            p.app.config_warning = Some("TUI config invalid; using defaults".into());
            for keys in ["", "G", "\t\t", "Gk", "ggjjjjj"] {
                p.keys(keys);
                let screen = p.text();
                assert!(
                    screen.lines().any(|l| l.starts_with('▌')),
                    "{height} after {keys:?}:\n{screen}"
                );
            }
        }
    }

    #[test]
    fn a_campaign_is_read_then_shown_and_follows_the_glance() {
        let mut p = Pane::new(120, 40);
        let sample: crate::model::Data =
            serde_json::from_str(include_str!("../tests/p1b-sample.json")).unwrap();
        p.app.data = sample.clone();
        // Under the ask, the empty checks card is an inert stop: `l` there reads nothing;
        // the next `j` is the campaign.
        p.keys("j");
        assert_eq!(
            glance::ids(&p.app.view())[p.app.row],
            glance::RowId::Card(1)
        );
        p.text();
        assert_eq!(press(&mut p.app, KeyCode::Char('l')), Effect::None);
        p.keys("j");
        assert_eq!(
            glance::ids(&p.app.view())[p.app.row],
            glance::RowId::Campaign(1)
        );
        // `l` asks for the read and shows nothing yet.
        let job = Job::Campaign {
            root: 1,
            name: "demo-checkout".into(),
            open: true,
            seq: 1,
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
                open: false,
                seq: 1,
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

    /// The fixture with its structured asks in front: the hub's dialog ask, a
    /// single-select, a multi-select.
    fn structured(width: u16, height: u16) -> Pane {
        let mut p = Pane::new(width, height);
        frames::with_structured(&mut p.app);
        p
    }

    #[test]
    fn a_structured_ask_answers_with_its_label_and_a_note() {
        let mut p = structured(46, 30);
        p.keys("ja");
        assert_eq!(p.app.screen, Screen::Answer);
        assert_eq!(p.app.text, "A: Ed25519");
        // Space picks nothing in a single-select ask.
        p.keys("j ");
        assert_eq!(p.app.text, "B: RSA-2048");
        // Tab types a note after the option; moving keeps it.
        p.keys("\tkeep the old keys a weekk");
        p.code(KeyCode::Backspace);
        assert_eq!(p.app.text, "B: RSA-2048 — keep the old keys a week");
        p.code(KeyCode::Down);
        assert_eq!(p.app.text, "C: HMAC-SHA256 — keep the old keys a week");
        // Esc sends nothing, and the next dialog starts clean.
        assert_eq!(p.keys("\x1b"), Effect::None);
        assert_eq!(p.app.screen, Screen::Glance);
        assert_eq!(p.status(), "cancelled, nothing was sent");
        p.keys("a");
        assert_eq!(p.app.text, "A: Ed25519");
    }

    #[test]
    fn a_multi_select_ask_sends_the_picked_set() {
        let mut p = structured(120, 40);
        let ask = p.app.data.glance.needs_you[2].clone();
        p.keys("jja");
        assert_eq!(p.app.screen, Screen::Answer);
        assert_eq!(p.app.text, "");
        // Nothing picked: Enter does nothing and says so.
        assert_eq!(p.keys("\n"), Effect::None);
        assert_eq!(p.app.screen, Screen::Answer);
        assert!(p.status().contains("pick at least one"), "{}", p.status());
        // Space picks English, German and Portuguese, then drops English.
        p.keys(" j jj kkk ");
        assert_eq!(p.app.picked, [1, 3]);
        assert_eq!(p.app.text, "B: German; D: Portuguese");
        assert!(p.text().contains("[x] B  German"));
        p.keys("\tglossary first");
        assert_eq!(p.app.text, "B: German; D: Portuguese — glossary first");
        p.keys("\n");
        assert_eq!(p.app.screen, Screen::Confirm);
        assert_eq!(
            answered(p.keys("y")).1,
            (
                ask.ask_id,
                "B: German; D: Portuguese — glossary first".into(),
                false
            )
        );
    }

    #[test]
    fn a_structured_ask_answered_elsewhere_is_not_retargeted() {
        let mut p = structured(120, 40);
        let ask = p.app.data.glance.needs_you[2].clone();
        p.keys("jja \n");
        assert_eq!(p.app.screen, Screen::Confirm);
        // A failed read is not evidence: the dialog still sends.
        p.app.fetch.error = Some("server unreachable".into());
        assert!(!p.app.gone);
        // The next successful read no longer lists it: answered or withdrawn elsewhere.
        let mut next = p.app.data.glance.clone();
        next.needs_you.retain(|a| a.ask_id != ask.ask_id);
        p.app.snapshot(next);
        let text = p.text();
        assert!(text.contains("is no longer open"), "{text}");
        // Only keys that do something are offered: no `y send`.
        assert!(
            text.contains("e edit") && text.contains("esc close") && !text.contains("y send "),
            "{text}"
        );
        // `y` sends nothing, and the draft stays until esc.
        assert_eq!(p.keys("y"), Effect::None);
        assert_eq!(p.app.screen, Screen::Confirm);
        assert!(p.status().contains("nothing was sent"), "{}", p.status());
        assert_eq!(p.app.text, "A: English");
        p.keys("e");
        assert_eq!(p.keys("\n"), Effect::None);
        assert_eq!(p.keys("y"), Effect::None);
        p.keys("\x1b");
        assert_eq!(p.app.screen, Screen::Glance);
        // A new dialog starts clean.
        p.app.snapshot(p.app.data.glance.clone());
        p.keys("ga");
        assert!(!p.app.gone);
    }

    #[test]
    fn a_recorded_answer_keeps_its_event_and_a_refused_one_its_reason() {
        let mut p = Pane::new(46, 30);
        let id = p.app.data.glance.needs_you[0].ask_id;
        let (job, _) = answered(p.keys("a\ny"));
        // Recorded, though the prompt did not reach the pane: the ledger's event, not the ask.
        let receipt = Done::Answered {
            answer_id: 777,
            note: "not delivered to its pane: no pane".into(),
        };
        done(&mut p.app, &job, Ok(receipt));
        assert_eq!(
            p.status(),
            format!("✓ answered {id} as event 777 · not delivered to its pane: no pane")
        );
        assert!(matches!(
            p.app.outcome(id).unwrap().state,
            Outcome::Recorded { answer_id: 777, .. }
        ));
        // It cannot be sent again from here.
        assert_eq!(p.keys("a"), Effect::None);
        assert_eq!(p.app.screen, Screen::Glance);
        assert!(
            p.status()
                .contains("already answered from here (event 777)")
        );

        // A refusal in the ledger's words: not recorded, the reason holds, no resend. The
        // answer is kept to read in full, wide and narrow, and nothing sends it.
        for (width, height) in [(120, 40), (46, 30)] {
            let mut p = Pane::new(width, height);
            p.app.fetch.live = Some(false);
            p.keys("a\t");
            p.keys(" and keep the glossary");
            let (job, (_, text, _)) = answered(p.keys("\ny"));
            let reason = format!("ask {id} is already answered by event 6");
            done(&mut p.app, &job, Ok(Done::Refused(reason.clone())));
            assert_eq!(p.status(), format!("! not recorded: {reason}"));
            let full = p.app.data.glance.clone();
            p.app.fetch.began = Some(Instant::now() + Duration::from_millis(1));
            p.app.snapshot(full);
            assert_eq!(p.keys("ga"), Effect::None);
            assert_eq!(
                (p.app.screen, p.app.text.as_str()),
                (Screen::Confirm, text.as_str())
            );
            let shown = p.text();
            assert!(
                shown.contains("keep the glossary")
                    && shown.contains("did not record")
                    && shown.contains("esc close")
                    && !shown.contains("y send "),
                "{width}x{height}\n{shown}"
            );
            // Neither y nor e sends or edits it; the job and its state stay as they were.
            for key in ["y", "e"] {
                assert_eq!(p.keys(key), Effect::None);
                assert_eq!(p.app.screen, Screen::Confirm);
                assert!(p.status().contains("not sent again"), "{}", p.status());
            }
            assert_eq!(p.app.outcomes.len(), 1);
            assert_eq!(p.app.outcomes[0].job, job);
            assert!(matches!(p.app.outcomes[0].state, Outcome::Refused(_)));
            p.keys("\x1b");
            assert_eq!(p.app.screen, Screen::Glance);
            assert!(!p.status().contains("nothing was sent"), "{}", p.status());
        }
    }

    #[test]
    fn a_refused_answer_stays_readable_in_the_picker_after_its_ask_leaves() {
        for (width, height) in [(120, 40), (46, 30)] {
            let mut p = Pane::new(width, height);
            let ask = p.app.data.glance.needs_you[0].clone();
            let mut without = p.app.data.glance.clone();
            without.needs_you.retain(|a| a.ask_id != ask.ask_id);
            // A long answer of its own, its end marked.
            p.keys("a");
            let options = ask::parse(&ask).options.len();
            while p.app.choice < options {
                p.code(KeyCode::Down);
            }
            let long = format!("{}END-OF-DRAFT", "word ".repeat(400));
            paste(&mut p.app, &long);
            let (job, (_, text, _)) = answered(p.keys("\ny"));
            assert_eq!(text, long);
            // Refused, then a snapshot begun after it no longer lists the ask.
            done(
                &mut p.app,
                &job,
                Ok(Done::Refused("answered concurrently".into())),
            );
            p.app.fetch.began = Some(Instant::now() + Duration::from_millis(1));
            p.app.snapshot(without.clone());
            // The picker lists it after the open asks, under its own label; the pill and
            // the open count do not.
            p.keys("G");
            let row = p.app.row;
            p.keys("a");
            assert_eq!(p.app.screen, Screen::Picker);
            let shown = p.text();
            assert!(
                shown.contains("1 kept answer") && shown.contains("NOT RECORDED"),
                "{width}x{height}\n{shown}"
            );
            assert_eq!(p.app.pickable().len(), without.needs_you.len() + 1);
            p.keys("G");
            assert!(p.app.picked_kept());
            assert_eq!(p.app.picked().map(|a| a.ask_id), Some(ask.ask_id));
            // A reorder keeps the highlight on it.
            let mut reordered = without.clone();
            reordered.needs_you.reverse();
            p.app.snapshot(reordered);
            assert_eq!(p.app.picked().map(|a| a.ask_id), Some(ask.ask_id));
            let shown = p.text();
            let keys = shown
                .lines()
                .find(|l| l.contains("⏎ read"))
                .unwrap_or_default();
            assert!(
                keys.contains("esc close") && !keys.contains("A Asks"),
                "{width}x{height}\n{shown}"
            );
            // `A` has nothing to show for it; ⏎ reads it, in full, as it was sent.
            assert_eq!(p.keys("A"), Effect::None);
            assert_eq!(p.app.screen, Screen::Picker);
            assert_eq!(p.keys("\n"), Effect::None);
            assert_eq!(
                (p.app.screen, p.app.text.as_str()),
                (Screen::Confirm, long.as_str())
            );
            let top = p.text();
            assert!(
                top.contains("did not record")
                    && top.contains("ctrl-d ctrl-u scroll")
                    && !top.contains("END-OF-DRAFT"),
                "{width}x{height}\n{top}"
            );
            for _ in 0..40 {
                p.code(KeyCode::PageDown);
            }
            let end = p.text();
            assert!(end.contains("END-OF-DRAFT"), "{width}x{height}\n{end}");
            // Nothing sends it, and esc returns to where the picker was opened.
            assert_eq!(p.keys("y"), Effect::None);
            assert!(matches!(
                p.app.outcome(ask.ask_id).unwrap().state,
                Outcome::Refused(_)
            ));
            p.keys("\x1b");
            assert_eq!((p.app.screen, p.app.row), (Screen::Glance, row));
        }
    }

    #[test]
    fn an_unknown_answer_waits_for_a_read_begun_after_it_and_resends_the_same_job() {
        let mut p = Pane::new(46, 30);
        let id = p.app.data.glance.needs_you[0].ask_id;
        let full = p.app.data.glance.clone();
        let mut without = full.clone();
        without.needs_you.retain(|a| a.ask_id != id);
        let (job, sent) = answered(p.keys("a\ny"));
        // In flight: a second `a` (or `y`) cannot send it twice.
        assert_eq!(p.keys("a"), Effect::None);
        assert!(p.status().contains("on its way"), "{}", p.status());
        let older = Instant::now();
        done(&mut p.app, &job, Err("taskr did not answer in 10s".into()));
        // The copy is fixed: no reason from the child reaches it.
        assert_eq!(
            p.status(),
            format!("? ask {id}: answer outcome unknown; checking the ledger")
        );
        let unknown =
            |p: &Pane| matches!(p.app.outcome(id).unwrap().state, Outcome::Unknown { .. });
        // Nothing repeats it, and `a` opens no new draft while it is checked.
        assert_eq!(p.keys("a"), Effect::None);
        assert_eq!(p.app.screen, Screen::Glance);
        assert!(p.status().contains("checking the ledger"));
        // A read begun before the failure proves nothing, even one without the ask; so
        // does a read with no start time.
        p.app.fetch.began = Some(older);
        p.app.snapshot(without.clone());
        assert!(unknown(&p));
        p.app.snapshot(full.clone());
        assert!(unknown(&p));
        // A read begun after it still lists the ask: the same answer, offered for `y`.
        p.app.fetch.began = Some(Instant::now() + Duration::from_millis(1));
        p.app.snapshot(full.clone());
        assert_eq!(p.app.outcome(id).unwrap().state, Outcome::Open);
        assert!(p.status().contains("still open"), "{}", p.status());
        p.keys("ga");
        assert_eq!(
            (p.app.screen, p.app.text.as_str()),
            (Screen::Confirm, sent.1.as_str())
        );
        // It is resent unchanged: `e` does not edit it, esc keeps it.
        p.keys("e");
        assert_eq!(p.app.screen, Screen::Confirm);
        assert!(p.status().contains("same answer"), "{}", p.status());
        p.keys("\x1b");
        assert!(
            p.status()
                .contains("no resend was sent; the answer is kept")
                && !p.status().contains("nothing was sent"),
            "{}",
            p.status()
        );
        p.keys("ga");
        // The resend is the identical job: ask, text, prompt flag and request key.
        assert_eq!(p.keys("y"), Effect::Run(job.clone()));
        assert_eq!(p.app.outcome(id).unwrap().state, Outcome::Sending);
        assert_eq!(p.app.outcomes.len(), 1);
        // Unknown again, and a later read no longer lists it: never claimed as ours.
        done(&mut p.app, &job, Err("transport".into()));
        p.app.fetch.began = Some(Instant::now() + Duration::from_millis(1));
        p.app.snapshot(without);
        assert_eq!(p.app.outcome(id).unwrap().state, Outcome::Gone);
        assert_eq!(
            p.status(),
            format!(
                "ask {id} {} is no longer open (answered or withdrawn elsewhere)",
                full.needs_you[0].campaign
            )
        );
    }

    #[test]
    fn a_resend_whose_ask_closes_under_it_sends_nothing_and_claims_nothing() {
        let mut p = Pane::new(120, 40);
        let id = p.app.data.glance.needs_you[0].ask_id;
        let full = p.app.data.glance.clone();
        let mut without = full.clone();
        without.needs_you.retain(|a| a.ask_id != id);
        let (job, _) = answered(p.keys("a\ny"));
        done(&mut p.app, &job, Err("transport".into()));
        p.app.fetch.began = Some(Instant::now() + Duration::from_millis(1));
        p.app.snapshot(full);
        p.keys("ga");
        assert!(p.app.resend && p.app.screen == Screen::Confirm);
        // The next successful read no longer lists it, while the resend is on screen.
        p.app.fetch.began = Some(Instant::now() + Duration::from_millis(2));
        p.app.snapshot(without);
        assert!(p.app.gone);
        assert_eq!(p.keys("y"), Effect::None);
        let status = p.status().to_string();
        assert!(
            status.contains("no resend was sent")
                && status.contains("unknown")
                && !status.contains("nothing was sent"),
            "{status}"
        );
        let shown = p.text();
        assert!(
            shown.contains("esc close")
                && !shown.contains("y resend")
                && !shown.contains("still open"),
            "{shown}"
        );
        // The job, its key and the draft are kept, and the screen stays until esc.
        assert_eq!(p.app.screen, Screen::Confirm);
        assert_eq!(p.app.outcomes.len(), 1);
        assert_eq!(p.app.outcomes[0].job, job);
        assert_eq!(p.app.outcomes[0].state, Outcome::Gone);
        assert_eq!(
            p.app.text,
            match &job {
                Job::Answer { text, .. } => text.clone(),
                _ => unreachable!(),
            }
        );
        p.keys("\x1b");
        assert!(!p.status().contains("nothing was sent"), "{}", p.status());
    }

    #[test]
    fn only_the_producers_start_time_settles_an_unknown_answer() {
        use crate::client::{Live, Pace, Update};
        let (tx, rx) = std::sync::mpsc::channel();
        let mut live = Live::new(rx, Pace::default(), None);
        let mut p = Pane::new(46, 30);
        let id = p.app.data.glance.needs_you[0].ask_id;
        let full = p.app.data.glance.clone();
        let (job, _) = answered(p.keys("a\ny"));
        // A read begun before the failure arrives after it: its arrival proves nothing.
        tx.send(Update::Started(Instant::now())).unwrap();
        done(&mut p.app, &job, Err("transport".into()));
        tx.send(Update::Done(Box::new(Ok(full.clone())))).unwrap();
        live.poll(&mut p.app, Instant::now() + Duration::from_secs(5));
        assert!(matches!(
            p.app.outcome(id).unwrap().state,
            Outcome::Unknown { .. }
        ));
        // A failed read begun later proves nothing either.
        tx.send(Update::Started(Instant::now() + Duration::from_millis(1)))
            .unwrap();
        tx.send(Update::Done(Box::new(Err("timeout".into()))))
            .unwrap();
        live.poll(&mut p.app, Instant::now());
        assert!(matches!(
            p.app.outcome(id).unwrap().state,
            Outcome::Unknown { .. }
        ));
        // A successful read begun after it settles it.
        tx.send(Update::Started(Instant::now() + Duration::from_millis(1)))
            .unwrap();
        tx.send(Update::Done(Box::new(Ok(full)))).unwrap();
        live.poll(&mut p.app, Instant::now());
        assert_eq!(p.app.outcome(id).unwrap().state, Outcome::Open);
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
        // Wide enough for every Dashboard hint and the mode together.
        let mut p = Pane::new(140, 30);
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
        // Where the hints fill the line, the mode gives way: every hint still shows.
        let mut p = Pane::new(100, 30);
        p.app.fetch.live = Some(false);
        let line = last(&mut p);
        assert!(
            !line.ends_with(" polling")
                && line.contains("D A F I screens")
                && line.contains("? help"),
            "{line}"
        );
        // Narrow: the mode gives way before any hint, and `? help` stays.
        let mut p = Pane::new(46, 30);
        p.app.fetch.live = Some(false);
        let line = last(&mut p);
        assert!(
            !line.ends_with(" polling") && line.contains("? help"),
            "{line}"
        );
        // A dialog's keys all show while polling: the editor, the review, the picker.
        for (width, height) in [(46, 30), (46, 20), (40, 12)] {
            let mut p = Pane::new(width, height);
            p.app.fetch.live = Some(false);
            p.keys("a");
            let editor = last(&mut p);
            assert_eq!(p.app.screen, Screen::Answer);
            if width >= 46 {
                assert!(editor.contains("⏎ review"), "{width}x{height} {editor}");
            }
            p.keys("\n");
            let review = last(&mut p);
            assert!(review.contains("y send"), "{width}x{height} {review}");
            if width >= 46 {
                assert!(
                    review.contains("e edit") && review.contains("esc"),
                    "{width}x{height} {review}"
                );
            }
            p.keys("\x1bGa");
            assert_eq!(p.app.screen, Screen::Picker);
            let picker = last(&mut p);
            assert!(picker.contains("⏎ answer"), "{width}x{height} {picker}");
            if width >= 46 {
                assert!(picker.contains("A Asks"), "{width}x{height} {picker}");
            }
        }
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
        // s from that campaign goes back to slotr, so l and s loops do not stack.
        p.keys("s");
        assert_eq!((p.app.screen, p.app.back.len()), (Screen::Slotr, 1));
        p.keys("ls");
        assert_eq!((p.app.screen, p.app.back.len()), (Screen::Slotr, 1));
        // From a campaign opened on the glance, s opens slotr and s again closes back to it.
        p.keys("h");
        p.app.go(Screen::Campaign);
        p.keys("s");
        assert_eq!(p.app.screen, Screen::Slotr);
        p.keys("s");
        assert_eq!(p.app.screen, Screen::Campaign);
        p.keys("h");
        assert_eq!(p.app.screen, Screen::Glance);
    }

    #[test]
    fn the_slotr_cursor_follows_its_run_across_reads() {
        let mut p = Pane::new(80, 24);
        p.keys("s");
        demo(&mut p.app, Job::Slotr);
        p.click_on("auth-rotation");
        assert_eq!(p.app.row, 4);
        // A runtime holder above the cursor ends; the next read keeps the cursor on its run.
        let mut next = p.app.data.slotr.clone();
        next.pools.get_mut("runtime").unwrap().holders.remove(0);
        done(&mut p.app, &Job::Slotr, Ok(Done::Slotr(Box::new(next))));
        assert_eq!(p.app.row, 3);
        assert_eq!(
            slotr::rows(&p.app.data.slotr)[p.app.row].campaign,
            "auth-rotation"
        );
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
        assert!(p.text().contains(" ! slotr: no systemd"));
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
        // An older taskr on either end does not know the command.
        let old = "taskr: unknown command slotr".to_string();
        done(&mut p.app, &Job::Slotr, Err(old));
        assert!(p.text().contains("older than taskr slotr: update both"));
    }

    fn row_of(p: &Pane, id: &glance::RowId) -> usize {
        glance::ids(&p.app.view())
            .iter()
            .position(|r| r == id)
            .unwrap_or_else(|| panic!("{id:?} is not a row"))
    }

    fn drawn(p: &Pane, i: usize) -> bool {
        p.app
            .seen
            .borrow()
            .hits
            .iter()
            .any(|(_, hit)| *hit == Hit::Row(i))
    }

    /// `l` on a campaign row, as the owner presses it; the read is not served.
    fn ask_read(p: &mut Pane, id: i64) -> Job {
        p.app.row = row_of(p, &glance::RowId::Campaign(id));
        p.text();
        match press(&mut p.app, KeyCode::Char('l')) {
            Effect::Run(job) => job,
            other => panic!("{other:?}"),
        }
    }

    fn read(id: i64, name: &str, decision: &str) -> Result<Done, String> {
        let mut c = crate::model::Campaign::default();
        (c.root.id, c.root.name) = (id, name.into());
        c.decisions = vec![crate::model::Event {
            id: 7,
            kind: "decision".into(),
            text: decision.into(),
            ..Default::default()
        }];
        Ok(Done::Campaign(Box::new(c)))
    }

    #[test]
    fn ledger_panels_scope_only_a_deliberate_open_and_age_by_its_read() {
        let now = 1_792_000_000;
        let mut p = Pane::new(120, 40);
        p.app.observed_at = now;
        for n in ['7', '8', '9'] {
            assert_eq!(p.code(KeyCode::Char(n)), Effect::SaveConfig);
        }
        assert_eq!(p.app.config.panels[6..9], [true; 3]);
        let text = p.text();
        assert!(text.contains("Select a campaign and press l"), "{text}");
        assert!(text.contains("7 Decisions+ · 8 Fleet+ · 9 Log+"), "{text}");
        // Moving over campaigns neither scopes nor reads; Glance r reads only the glance.
        for _ in 0..12 {
            assert_eq!(p.code(KeyCode::Char('j')), Effect::None);
        }
        assert_eq!(p.code(KeyCode::Char('r')), Effect::Refresh);
        assert!(p.app.scope.read_at.is_none() && follow(&p.app).is_none());
        let scoped = |p: &Pane| {
            glance::ids(&p.app.view())
                .iter()
                .any(|r| matches!(r, glance::RowId::Decision(..) | glance::RowId::Logged(..)))
        };
        assert!(!scoped(&p));
        // The demo serves the read: `l` shows the campaign, Esc returns to the panels.
        p.app.config.panels[6..10].fill(false);
        p.app.row = row_of(&p, &glance::RowId::Campaign(4120));
        p.keys("l");
        assert_eq!(p.app.screen, Screen::Campaign);
        p.keys("\x1b");
        p.app.config.panels[6..10].fill(true);
        assert_eq!(
            (p.app.screen, p.app.scope.read_at),
            (Screen::Glance, Some(now))
        );
        assert!(p.text().contains("tui-frames #4120 · read 0s ago"));
        let ids = glance::ids(&p.app.view());
        // The same event in force and in the log sample: two identities.
        assert!(ids.contains(&glance::RowId::Decision(4120, 61301)));
        assert!(ids.contains(&glance::RowId::Logged(4120, 61301)));
        assert!(follow(&p.app).is_none());
        // The panels' age follows the read, and a new minute asks for a frame.
        assert!(p.app.advance_time(now + 120, Instant::now()));
        assert!(p.text().contains("read 2m ago"));
        assert!(!p.app.advance_time(now + 125, Instant::now()));
        p.app.config.panels[6] = false;
        p.app.config.panels[8] = false;
        assert!(!p.app.advance_time(now + 300, Instant::now()));
    }

    #[test]
    fn superseded_mismatched_cancelled_and_failed_reads_never_retarget() {
        let mut p = Pane::new(120, 40);
        p.app.data.campaign = crate::model::Campaign::default();
        let first = ask_read(&mut p, 4310);
        let second = ask_read(&mut p, 4288);
        // The earlier open's reply lands after a later one was asked for: dropped.
        done(&mut p.app, &first, read(4310, "search-index", "old"));
        assert_eq!(
            (
                p.app.screen,
                p.app.scope.read_at,
                p.app.data.campaign.root.id
            ),
            (Screen::Glance, None, 0)
        );
        // A reply for another campaign than the one asked for is an error.
        done(&mut p.app, &second, read(999, "other", "wrong"));
        assert_eq!(
            (p.app.screen, p.app.data.campaign.root.id),
            (Screen::Glance, 0)
        );
        assert!(p.status().starts_with("! read auth-rotation failed"));
        assert!(
            p.app
                .scope
                .error
                .as_deref()
                .unwrap()
                .starts_with("reading auth-rotation failed")
        );
        // The owner moved on to the help: the reply shows nothing and changes nothing.
        let third = ask_read(&mut p, 4288);
        p.keys("?");
        done(
            &mut p.app,
            &third,
            read(4288, "auth-rotation", "keep the cache"),
        );
        assert_eq!(
            (
                p.app.screen,
                p.app.scope.read_at,
                p.app.data.campaign.root.id
            ),
            (Screen::Help, None, 0)
        );
        assert!(p.status().contains("l opens it"), "{}", p.status());
        p.keys("\x1b");
        let fourth = ask_read(&mut p, 4288);
        done(
            &mut p.app,
            &fourth,
            read(4288, "auth-rotation", "keep the cache"),
        );
        assert_eq!(p.app.screen, Screen::Campaign);
        assert!(p.app.scope.read_at.is_some() && p.app.scope.error.is_none());
        p.keys("\x1b");
        p.app.config.panels[6] = true;
        let text = p.text();
        assert!(text.contains("auth-rotation #4288") && text.contains("keep the cache"));
        // A failed read again keeps the last good one on hand, and says so.
        let read_at = p.app.scope.read_at;
        let fifth = ask_read(&mut p, 4288);
        p.keys("\x1b");
        done(&mut p.app, &fifth, Err("boom".into()));
        assert_eq!(p.app.scope.read_at, read_at);
        let text = p.text();
        assert!(
            text.contains("reading auth-rotation failed: boom") && text.contains("keep the cache"),
            "{text}"
        );
        // A refresh asked for before the latest open cannot replace it.
        let stale = Job::Campaign {
            root: 4288,
            name: "auth-rotation".into(),
            open: false,
            seq: 1,
        };
        done(&mut p.app, &stale, read(4288, "auth-rotation", "stale"));
        assert!(p.text().contains("keep the cache"));
        assert_eq!(p.app.screen, Screen::Glance);
    }

    #[test]
    fn informational_rows_only_open_their_campaign() {
        let mut p = Pane::new(120, 40);
        p.app.config.panels[6..10].fill(true);
        p.app.scope.read_at = Some(p.app.observed_at);
        let ids = glance::ids(&p.app.view());
        let rows = [
            ids.iter()
                .find(|r| matches!(r, glance::RowId::Decision(..))),
            ids.iter().find(|r| matches!(r, glance::RowId::Fleet(_))),
            ids.iter().find(|r| matches!(r, glance::RowId::Logged(..))),
        ];
        for id in rows.map(|r| r.unwrap().clone()) {
            p.app.row = row_of(&p, &id);
            p.text();
            assert!(drawn(&p, p.app.row), "{id:?}");
            let hints = glance::hints(&p.app.view(), true);
            assert!(hints.iter().any(|(k, _)| *k == "o"), "{hints:?}");
            // `a` here only lists the open asks (the picker); it never answers this row.
            assert!(
                !hints
                    .iter()
                    .any(|h| ["y", "⏎", "p"].contains(&h.0) || (h.0 == "a" && h.1 != "asks")),
                "{hints:?}"
            );
            for code in [
                KeyCode::Enter,
                KeyCode::Right,
                KeyCode::Char('p'),
                KeyCode::Char('l'),
                KeyCode::Char('y'),
            ] {
                p.text();
                assert_eq!(press(&mut p.app, code), Effect::None, "{id:?} {code:?}");
                assert!(p.status().contains("informational"), "{}", p.status());
            }
            assert!(p.app.pending.is_none() && p.app.ask.is_none());
            p.text();
            a_lists_asks(&mut p);
            assert_eq!(p.app.screen, Screen::Glance);
            p.text();
            let Effect::Run(Job::Campaign { root, .. }) = press(&mut p.app, KeyCode::Char('o'))
            else {
                panic!("o reads the row's campaign")
            };
            let want = match id {
                glance::RowId::Fleet(root) => root,
                _ => 4120,
            };
            assert_eq!(root, want);
            if p.app.screen != Screen::Glance {
                p.app.close();
            }
        }
    }

    #[test]
    fn fleet_reads_the_listed_snapshot_whatever_panel_3_shows() {
        let mut p = Pane::new(120, 40);
        p.app.config.panels[2] = false;
        p.app.config.panels[7] = true;
        let listed = p.app.data.glance.campaigns.len();
        p.app.data.glance.campaigns[0].lanes.known = [true, false, false];
        let ids = glance::ids(&p.app.view());
        assert!(!ids.iter().any(|r| matches!(r, glance::RowId::Campaign(_))));
        let fleet = ids
            .iter()
            .filter(|r| matches!(r, glance::RowId::Fleet(_)))
            .count();
        assert_eq!(fleet, listed);
        let text = p.text();
        assert!(
            text.contains("FLEET") && text.contains(&format!("{listed} listed")),
            "{text}"
        );
        // A count the snapshot left out is unknown, in the row and in the totals.
        assert!(
            text.contains("+? ready") && text.contains("? ready ? open"),
            "{text}"
        );
        p.keys("/auth\n");
        assert!(p.text().contains("filtered: 1 listed"));
        p.keys("\x1b");
        assert!(p.app.filter.is_empty());
        p.app.fetch.error = Some("hub unreachable".into());
        assert!(p.text().contains("! ledger stale"));
        // No read: its Fleet rows go, and the cursor with them as a snapshot would move it.
        p.app.fetch = crate::Fetch::default();
        p.app.row = 0;
        let text = p.text();
        assert!(
            text.contains("Ledger not read yet; counts unknown"),
            "{text}"
        );
        p.app.fetch.loaded = true;
        p.app.data.glance.campaigns.clear();
        p.app.data.glance.quiet = Default::default();
        assert!(p.text().contains("not a fleet-wide idle claim"));
    }

    #[test]
    fn ledger_panels_preview_under_listed_asks_and_fill_an_empty_ledger() {
        let mut p = Pane::new(120, 40);
        p.app.config.panels[6..10].fill(true);
        p.app.scope.read_at = Some(p.app.observed_at);
        let decision = p.app.data.campaign.decisions[0].clone();
        p.app.data.campaign.decisions = (1..=14)
            .map(|id| crate::model::Event {
                id,
                ..decision.clone()
            })
            .collect();
        let text = p.text();
        // Owner asks and checks keep every row; 7-9 preview three rows each, and all show.
        for rule in ["NEEDS YOU", "TO CHECK"] {
            let line = text.lines().find(|l| l.contains(rule)).unwrap();
            assert!(!line.contains('/'), "{rule} is windowed:\n{text}");
        }
        for rule in ["DECISIONS · PLAN", "FLEET", "DECISION LOG"] {
            assert!(text.contains(rule), "{rule} missing:\n{text}");
        }
        // Cut by the band below the asks, each says what it leaves out.
        assert!(text.contains("more decisions · tab here"), "{text}");
        // With asks, checks and campaigns hidden Fleet fills the pane; the state keeps
        // only its own two lines and says what is hidden, not that no ledger rows show.
        let mut p = Pane::new(46, 30);
        p.app.config.panels[..3].fill(false);
        p.app.config.panels[7] = true;
        let text = p.text();
        assert!(
            text.contains("Asks, checks and campaigns hidden")
                && text.contains("1-3 show them")
                && !text.contains("No visible ledger rows"),
            "{text}"
        );
        assert!(
            text.contains("legacy-importer") && !text.contains(" more "),
            "{text}"
        );
        // A filter that empties asks, checks and campaigns says so, not that they are hidden.
        // Decisions stay under it: the filter leaves one campaign's decisions. Quiet
        // campaigns are not filtered and keep the CAMPAIGNS row, so the fixture has none.
        p.app.config.panels[..3].fill(true);
        p.app.config.panels[6] = true;
        p.app.data.glance.quiet.count = 0;
        p.app.filter = "no-such-campaign".into();
        let text = p.text();
        assert!(
            text.contains("No matching asks, checks or campaigns")
                && text.contains("Esc clears the filter")
                && text.contains("DECISIONS")
                && !text.contains("No visible ledger rows"),
            "{text}"
        );
    }

    #[test]
    fn hidden_ledger_switches_cost_the_narrow_glance_no_row() {
        let lines = |p: &mut Pane| p.text().lines().map(String::from).collect::<Vec<_>>();
        // Short, every switch shares one line under the tab line, labels left to the help.
        let mut p = Pane::new(46, 30);
        let text = lines(&mut p);
        assert_eq!(
            text[2], "panels 1+ 2+ 3+ 4+ 5+ 6+ 7- 8- 9- 0- H-",
            "{text:?}"
        );
        assert!(!text[3].contains('+'), "{text:?}");
        assert_eq!(p.code(KeyCode::Char('8')), Effect::SaveConfig);
        assert_eq!(lines(&mut p)[2], "panels 1+ 2+ 3+ 4+ 5+ 6+ 7- 8+ 9- 0- H-");
        // Taller, the named switches: the hidden five fold into one entry.
        let mut p = Pane::new(46, 32);
        let text = lines(&mut p);
        assert_eq!(
            text[3], "4 PRs+ · 5 Shipped+ · 6 Build+ · 7-9,0,H off",
            "{text:?}"
        );
        assert!(!text[4].contains("Fleet"), "{text:?}");
        assert_eq!(p.code(KeyCode::Char('8')), Effect::SaveConfig);
        let text = lines(&mut p);
        assert_eq!(
            text[4], "7 Decisions- · 8 Fleet+ · 9 Log- · 0 Issues-",
            "{text:?}"
        );
        // H wraps to a line of its own at 46 columns: no hidden switch reads as enabled.
        assert_eq!(text[5], "H Health-", "{text:?}");
    }

    #[test]
    fn a_snapshot_under_the_help_keeps_the_fleet_selection() {
        let mut p = Pane::new(120, 40);
        p.app.config.panels[7] = true;
        let id = glance::RowId::Fleet(4288);
        p.app.row = row_of(&p, &id);
        p.keys("?");
        let mut snapshot = p.app.data.glance.clone();
        snapshot.campaigns.reverse();
        p.app.snapshot(snapshot);
        p.keys("\x1b");
        assert_eq!(glance::ids(&p.app.view())[p.app.row], id);
    }

    #[test]
    fn every_ledger_identity_is_reachable_and_earlier_panels_keep_priority() {
        for width in [46, 120] {
            for height in 7..=40 {
                let mut p = Pane::new(width, height);
                p.app.mine = crate::prs::demo(p.app.observed_at);
                p.app.shipped = crate::github::demo_shipped(p.app.observed_at);
                p.app.builds = crate::github::demo_builds(p.app.observed_at);
                p.app.config.panels[6..10].fill(true);
                p.app.scope.read_at = Some(p.app.observed_at);
                let ids = glance::ids(&p.app.view());
                for id in [
                    glance::RowId::Decision(4120, 61301),
                    glance::RowId::Fleet(4120),
                    glance::RowId::Logged(4120, 61301),
                ] {
                    assert!(ids.contains(&id), "{id:?}");
                }
                for (i, id) in ids.iter().enumerate() {
                    p.app.row = i;
                    let text = p.text();
                    if height >= 8 {
                        assert!(drawn(&p, i), "{width}x{height}/{id:?}:\n{text}");
                    }
                    // A lower card never shows while a higher one is gone unless the ↑ line
                    // says what is above (band order; tiny frames have no rules).
                    let order = [
                        "NEEDS YOU",
                        "TO CHECK",
                        "MY PRS",
                        "CAMPAIGNS",
                        "SHIPPED",
                        "BUILD QUEUE",
                        "DECISIONS · PLAN",
                        "DECISION LOG",
                        "FLEET",
                    ];
                    let rule = |name: &str| text.contains(&format!(" {name} ─"));
                    // In two columns a later right-hand card can sit beside an earlier one
                    // cut below: then the ↓ line names it.
                    let cue = text
                        .lines()
                        .any(|l| l.starts_with(" ↑ ") || l.starts_with(" ↓ "));
                    for (later, name) in order.iter().enumerate().skip(1) {
                        if rule(name) {
                            for earlier in &order[..later] {
                                assert!(
                                    rule(earlier) || cue,
                                    "{width}x{height} row {i}: {name} without {earlier}:\n{text}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn issues_rows_are_informational_reachable_last_and_expire_without_fetch() {
        let issues = |p: &mut Pane| {
            p.app.mine = crate::prs::demo(p.app.observed_at);
            p.app.shipped = crate::github::demo_shipped(p.app.observed_at);
            p.app.builds = crate::github::demo_builds(p.app.observed_at);
            p.app.issues = crate::github::demo_issues(p.app.observed_at);
        };
        // Default narrow: Issues hidden, the switches on their one short line.
        let mut p = Pane::new(46, 30);
        issues(&mut p);
        let text = p.text();
        assert!(
            text.contains("7- 8- 9- 0- H-") && !text.contains("ISSUES"),
            "{text}"
        );
        assert!(
            !glance::ids(&p.app.view())
                .iter()
                .any(|r| matches!(r, glance::RowId::Assignee(..)))
        );
        assert_eq!(p.code(KeyCode::Char('0')), Effect::SaveConfig);
        assert!(p.app.config.panels[9] && p.app.config.panels[..6] == [true; 6]);
        // Shown, but below the earlier panels' priority in a full narrow pane: its rows
        // are identities the cursor reaches (checked below), not a guaranteed preview.
        let text = p.text();
        assert!(text.contains("9- 0+ H-"), "{text}");
        assert!(
            glance::ids(&p.app.view())
                .iter()
                .any(|r| matches!(r, glance::RowId::Assignee(..)))
        );
        for width in [46, 120] {
            for height in 7..=40 {
                let mut p = Pane::new(width, height);
                issues(&mut p);
                p.app.config.panels[6..10].fill(true);
                p.app.scope.read_at = Some(p.app.observed_at);
                let ids = glance::ids(&p.app.view());
                let unassigned = glance::RowId::Assignee("example-org/app".into(), None);
                assert!(ids.contains(&unassigned));
                // Issues come after every other card's rows (band D, after Fleet); Health
                // is off here, so they are last.
                let first = ids
                    .iter()
                    .position(|r| matches!(r, glance::RowId::Assignee(..)))
                    .unwrap();
                assert!(matches!(ids[first - 1], glance::RowId::Fleet(_)));
                assert!(
                    ids[first..]
                        .iter()
                        .all(|r| matches!(r, glance::RowId::Assignee(..)))
                );
                for (i, id) in ids.iter().enumerate() {
                    p.app.row = i;
                    let text = p.text();
                    if height >= 8 {
                        assert!(drawn(&p, i), "{width}x{height}/{id:?}:\n{text}");
                    }
                    // Shown below the cards before it, or with the ↑ line naming them.
                    if text.contains(" ISSUES ─") {
                        let above = text.lines().any(|l| l.starts_with(" ↑ "));
                        assert!(
                            above || (text.contains("NEEDS YOU") && text.contains("DECISION LOG")),
                            "{text}"
                        );
                    }
                    if matches!(id, glance::RowId::Assignee(..)) {
                        for code in [
                            KeyCode::Enter,
                            KeyCode::Right,
                            KeyCode::Char('p'),
                            KeyCode::Char('l'),
                            KeyCode::Char('y'),
                            KeyCode::Char('o'),
                        ] {
                            assert_eq!(
                                press(&mut p.app, code),
                                Effect::None,
                                "{width}x{height}/{id:?}"
                            );
                        }
                        a_lists_asks(&mut p);
                        assert!(p.app.ask.is_none() && p.app.pending.is_none());
                        assert_eq!(p.app.screen, Screen::Glance);
                    }
                }
            }
        }
        // The demo's closure leaves the window at its 24h boundary: a redraw, the same
        // group kept under the help, no fetch.
        let mut p = Pane::new(120, 40);
        issues(&mut p);
        // Only Issues can expire here, and it is the only external panel.
        p.app.mine = crate::prs::Mine::default();
        p.app.shipped = crate::github::Dashboard::default();
        p.app.builds = crate::github::Dashboard::default();
        p.app.config.panels[9] = true;
        let now = p.app.observed_at;
        let kim = glance::RowId::Assignee("example-org/app".into(), Some("kim".into()));
        let alex = glance::RowId::Assignee("example-org/app".into(), Some("alex".into()));
        // kim is the fourth group, past the three-row preview: the cursor brings it in.
        p.app.row = row_of(&p, &kim);
        assert!(
            p.text()
                .contains("@kim · 0 open · 0 blocked · 1 closed/24h")
        );
        p.app.row = row_of(&p, &alex);
        p.keys("?");
        assert!(!p.app.advance_time(now + 86400 - 3600, Instant::now()));
        assert!(p.app.advance_time(now + 86400 - 3599, Instant::now()));
        assert!(p.app.seen.borrow().hits.is_empty() && p.app.click.is_none());
        p.keys("\x1b");
        assert_eq!(glance::ids(&p.app.view())[p.app.row], alex);
        assert!(!glance::ids(&p.app.view()).contains(&kim));
        assert!(p.text().contains("0 closed/24h · unique"));
        // A surviving group that loses a closure also redraws.
        let mut p = Pane::new(120, 40);
        issues(&mut p);
        // Only Issues can expire here, and it is the only external panel.
        p.app.mine = crate::prs::Mine::default();
        p.app.shipped = crate::github::Dashboard::default();
        p.app.builds = crate::github::Dashboard::default();
        p.app.config.panels[9] = true;
        let s = &mut p.app.issues.sources[0];
        s.issues
            .iter_mut()
            .find(|i| i.number == 8)
            .unwrap()
            .assignees = vec!["alex".into()];
        let now = p.app.observed_at;
        assert!(p.app.advance_time(now + 86400 - 3599, Instant::now()));
        assert!(glance::ids(&p.app.view()).contains(&alex));
    }

    #[test]
    fn health_is_uppercase_h_informational_last_and_says_stale_first() {
        let health = |p: &mut Pane| {
            p.app.mine = crate::prs::demo(p.app.observed_at);
            p.app.shipped = crate::github::demo_shipped(p.app.observed_at);
            p.app.builds = crate::github::demo_builds(p.app.observed_at);
            p.app.issues = crate::github::demo_issues(p.app.observed_at);
            p.app.health = crate::health::demo(p.app.observed_at);
        };
        let row = glance::RowId::Health("example-org/app".into(), crate::health::ENV);
        // Default off, folded with 7-9 and 0; lowercase h stays back and toggles nothing.
        let mut p = Pane::new(46, 30);
        health(&mut p);
        assert!(p.text().contains("7- 8- 9- 0- H-") && !glance::ids(&p.app.view()).contains(&row));
        // The help names H: its View keys are on the help's last page.
        p.keys("?G");
        assert_eq!(p.app.screen, Screen::Help);
        let help = p.text();
        assert!(help.contains("toggle Health"), "{help}");
        assert_eq!(p.code(KeyCode::Char('h')), Effect::None);
        assert_eq!(p.app.screen, Screen::Glance);
        assert_eq!(p.code(KeyCode::Char('h')), Effect::None);
        assert!(!p.app.config.panels[10]);
        // H is Glance's only: on the campaign view it does nothing.
        p.app.go(Screen::Campaign);
        assert_eq!(p.code(KeyCode::Char('H')), Effect::None);
        p.app.close();
        assert!(!p.app.config.panels[10]);
        assert_eq!(p.code(KeyCode::Char('H')), Effect::SaveConfig);
        assert!(
            p.app.config.panels[10]
                && p.app.config.panels[..10]
                    == [
                        true, true, true, true, true, true, false, false, false, false
                    ]
        );
        let text = p.text();
        assert!(text.contains("0- H+"), "{text}");
        // Its row comes after Issues' and last of all (band D, last card).
        p.app.config.panels[9] = true;
        let ids = glance::ids(&p.app.view());
        let at = ids.iter().position(|r| *r == row).unwrap();
        assert!(matches!(ids[at - 1], glance::RowId::Assignee(..)));
        assert_eq!(at, ids.len() - 1);
        // Every height: the row is drawn, informational, and stale or unknown comes first.
        for width in [46, 120] {
            for height in 7..=40 {
                for state in 0..3 {
                    let mut p = Pane::new(width, height);
                    health(&mut p);
                    p.app.config.panels[6..].fill(true);
                    p.app.scope.read_at = Some(p.app.observed_at);
                    match state {
                        1 => p.app.health.error = Some(crate::health::UNAVAILABLE),
                        2 => {
                            p.app.health = crate::health::sample(p.app.observed_at, "no note");
                            p.app.health.error = Some(crate::health::UNAVAILABLE);
                        }
                        _ => {}
                    }
                    p.app.row = row_of(&p, &row);
                    let text = p.text();
                    if height >= 8 {
                        assert!(drawn(&p, p.app.row), "{width}x{height}:\n{text}");
                    }
                    let cue = ["as of ", "Stale · as of ", "Stale · Unknown · "][state];
                    if height >= 8 {
                        assert!(text.contains(cue), "{width}x{height}/{state}:\n{text}");
                    }
                    for code in [
                        KeyCode::Enter,
                        KeyCode::Right,
                        KeyCode::Char('p'),
                        KeyCode::Char('l'),
                        KeyCode::Char('y'),
                        KeyCode::Char('o'),
                    ] {
                        assert_eq!(press(&mut p.app, code), Effect::None, "{width}x{height}");
                    }
                    a_lists_asks(&mut p);
                    assert!(p.app.ask.is_none() && p.app.pending.is_none());
                    assert_eq!(p.app.screen, Screen::Glance);
                }
            }
        }
        // Narrow and short: the row puts the stale cue before what it clips, on the card's
        // row at 46x20 and on the tiny one-line card at 40x12.
        for (w, h, cue) in [
            (46, 20, "▌Stale · as of"),
            (40, 12, "▌Health · Stale · as of"),
        ] {
            let mut p = Pane::new(w, h);
            health(&mut p);
            p.app.config.panels[10] = true;
            p.app.health.error = Some(crate::health::UNAVAILABLE);
            p.app.row = row_of(&p, &row);
            let text = p.text();
            assert!(text.lines().any(|l| l.starts_with(cue)), "{w}x{h}:\n{text}");
        }
        // A double click selects and goes nowhere; a new report keeps the selection.
        let mut p = Pane::new(120, 40);
        health(&mut p);
        p.app.config.panels[10] = true;
        p.app.row = row_of(&p, &row);
        // Detail names the report; esc returns to its row.
        p.keys("I");
        let text = p.text();
        assert!(
            text.contains("Health · example-org/app production"),
            "{text}"
        );
        p.keys("\x1b");
        assert_eq!(glance::ids(&p.app.view())[p.app.row], row);
        assert_eq!(p.click_on("as of "), Effect::None);
        assert_eq!(p.click_on("as of "), Effect::None);
        assert!(p.app.ask.is_none() && p.app.pending.is_none() && p.app.status.is_none());
        p.app.health = crate::health::sample(
            p.app.observed_at,
            &crate::health::CLEAN.replace("deploy=live", "deploy=failed"),
        );
        assert_eq!(glance::ids(&p.app.view())[p.app.row], row);
        assert!(p.text().contains("deploy failed"));
        // Every panel hidden: the recovery line names H, and H brings Health back.
        p.app.config.panels = [false; 11];
        assert!(p.text().contains("1-9, 0 or H"));
        assert_eq!(p.code(KeyCode::Char('H')), Effect::SaveConfig);
        assert_eq!(glance::ids(&p.app.view()), [row]);
    }

    #[test]
    fn narrow_health_wraps_every_reported_value_with_one_cursor_line() {
        // Health alone at 46 columns: the expanded block wraps, nothing the note says is cut.
        let mut p = Pane::new(46, 30);
        p.app.config.panels = [false; 11];
        p.app.config.panels[10] = true;
        p.app.health = crate::health::sample(
            p.app.observed_at,
            &crate::health::CLEAN.replace("sentry=clean", "sentry=new:TRIP-12,TRIP-14"),
        );
        let row = glance::RowId::Health("example-org/app".into(), crate::health::ENV);
        p.app.row = row_of(&p, &row);
        let text = p.text();
        for value in ["0a1b2c3d4e", "new:TRIP-12,TRIP-14", "#7002", "#7000"] {
            assert!(text.contains(value), "{value}:\n{text}");
        }
        assert!(text.contains("checked "), "{text}");
        // Only the row's first line is the cursor's.
        let hits = p.app.seen.borrow().hits.clone();
        let lines = hits
            .iter()
            .filter(|(_, h)| *h == Hit::Row(p.app.row))
            .count();
        assert_eq!(lines, 1, "{text}");
        let first = text.lines().position(|l| l.contains("as of ")).unwrap();
        let (area, _) = hits
            .iter()
            .find(|(_, h)| *h == Hit::Row(p.app.row))
            .unwrap();
        assert_eq!(area.y as usize, first, "{text}");
        // Short of room it keeps its minimum: the rule, the source line and the row.
        for height in 8..=12 {
            let mut p = Pane::new(46, height);
            p.app.config.panels[10] = true;
            p.app.health = crate::health::demo(p.app.observed_at);
            p.app.row = row_of(&p, &row);
            let text = p.text();
            assert!(drawn(&p, p.app.row), "{height}:\n{text}");
            assert!(text.contains("as of "), "{height}:\n{text}");
        }
        // The wide default view keeps the row to one line: cut with `…`, or whole to the sha,
        // never a bare key.
        for name in ["health-120x40", "health-120x40-new"] {
            let spec = frames::all().into_iter().find(|s| s.name == name).unwrap();
            let text = frames::text(&frames::render(&spec));
            let row = text.lines().find(|l| l.contains("as of ")).unwrap();
            let end = row.trim_end_matches([' ', '│']);
            assert!(
                end.ends_with('…') || end.ends_with("sha 0a1b2c3d4e"),
                "{name}:\n{text}"
            );
        }
    }

    /// Every card on with every source full: the demo PRs, Shipped, Build, Issues and a
    /// Health report, panels 7 and 9 from a campaign read three minutes old.
    fn all_on(p: &mut Pane) {
        let app = &mut p.app;
        app.observed_at = 1_792_000_000;
        app.mine = crate::prs::demo(app.observed_at);
        app.shipped = crate::github::demo_shipped(app.observed_at);
        app.builds = crate::github::demo_builds(app.observed_at);
        app.issues = crate::github::demo_issues(app.observed_at);
        app.health = crate::health::sample(app.observed_at, crate::health::CLEAN);
        app.config.panels = [true; 11];
        app.scope.read_at = Some(app.observed_at - 180);
    }

    /// The rect the last frame drew for row `i`, if any.
    fn hit_rect(p: &Pane, i: usize) -> Option<ratatui::layout::Rect> {
        p.app
            .seen
            .borrow()
            .hits
            .iter()
            .find(|(_, hit)| *hit == Hit::Row(i))
            .map(|(r, _)| *r)
    }

    /// The selected row is drawn, inside the body, and its card's rule is on screen
    /// unless the frame is tiny; a click on it selects it.
    fn assert_reachable(p: &mut Pane, tiny: bool) {
        let screen = p.text();
        let (w, h) = (
            p.terminal.backend().buffer().area.width,
            p.terminal.backend().buffer().area.height,
        );
        let row = p.app.row;
        let rect =
            hit_rect(p, row).unwrap_or_else(|| panic!("{w}x{h}: row {row} not drawn:\n{screen}"));
        assert!(
            rect.y >= 2 && rect.y < h - 1,
            "{w}x{h}: row {row} at {rect:?}"
        );
        let line = screen.lines().nth(rect.y as usize).unwrap();
        assert!(!line.trim_start().starts_with(['↑', '↓']), "{line}");
        if !tiny {
            let id = &glance::ids(&p.app.view())[row];
            let title = glance::card(id);
            assert!(
                screen.lines().any(|l| l.contains(&format!(" {title} ─"))
                    || l.contains(&format!(" {title} · PLAN ─"))),
                "{w}x{h}: {title} rule not drawn for row {row}:\n{screen}"
            );
        }
        // No card is drawn as a bare rule: the line under a rule is the card's own.
        let lines: Vec<&str> = screen.lines().collect();
        for (y, l) in lines.iter().enumerate().take(h as usize - 1) {
            if l.contains(" ─") && (l.starts_with(' ') && !l.starts_with("  ")) {
                let next = lines.get(y + 1).copied().unwrap_or("");
                assert!(
                    y + 1 < h as usize - 1 && !next.starts_with(" ↓"),
                    "{w}x{h}: orphan rule at {y}:\n{screen}"
                );
            }
        }
        p.click(rect.x, rect.y);
        p.app.click = None;
        assert_eq!(p.app.row, row);
        // No hit target outside the body: the header, tab line and footer hold keys only.
        for (r, hit) in p.app.seen.borrow().hits.iter() {
            if matches!(hit, Hit::Row(_)) {
                assert!(r.y >= 2 && r.y < h - 1 && r.right() <= w, "{r:?}");
            }
        }
    }

    #[test]
    fn every_row_of_every_card_is_drawn_when_selected() {
        for (w, h) in [(46, 20), (46, 30), (80, 30), (120, 40), (40, 12)] {
            let tiny = w < 46 || h < 20;
            let mut p = Pane::new(w, h);
            all_on(&mut p);
            let n = glance::ids(&p.app.view()).len();
            assert!(n > 30, "{n}");
            for i in 0..n {
                assert_eq!(p.app.row, i, "{w}x{h}");
                assert_reachable(&mut p, tiny);
                p.code(KeyCode::Down);
            }
            assert_eq!(p.app.row, n - 1, "j stops at the end");
            for i in (0..n).rev() {
                assert_eq!(p.app.row, i, "{w}x{h}");
                assert_reachable(&mut p, tiny);
                p.code(KeyCode::Up);
            }
            // Tab lands on every card in band order, and back; g G and the wheel reveal too.
            let starts = glance::sections(&p.app.view());
            assert_eq!(starts.len(), 11, "{w}x{h}");
            for &start in starts.iter().skip(1).chain(&starts[..1]) {
                p.code(KeyCode::Tab);
                assert_eq!(p.app.row, start, "{w}x{h}");
                assert_reachable(&mut p, tiny);
            }
            for &start in starts.iter().rev() {
                p.code(KeyCode::BackTab);
                assert_eq!(p.app.row, start, "{w}x{h}");
                assert_reachable(&mut p, tiny);
            }
            p.keys("G");
            assert_eq!(p.app.row, n - 1);
            assert_reachable(&mut p, tiny);
            p.mouse(MouseEventKind::ScrollUp, 1, 5);
            assert_eq!(p.app.row, n - 2);
            assert_reachable(&mut p, tiny);
            p.keys("g");
            assert_eq!(p.app.row, 0);
            assert_reachable(&mut p, tiny);
        }
    }

    /// Only the first ask and Health have rows; every card between them is empty or
    /// unavailable. Each is an inert stop: Tab, j k and the disclosures show its state, and
    /// no key acts on it.
    #[test]
    fn empty_and_unavailable_cards_are_reached_and_never_act() {
        use crate::github::{Dashboard, Kind};
        for (w, h) in [(46, 20), (120, 40), (40, 12)] {
            let tiny = w < 46 || h < 20;
            let mut p = Pane::new(w, h);
            all_on(&mut p);
            let app = &mut p.app;
            app.data.glance.needs_you.truncate(1);
            app.data.glance.attention.clear();
            app.data.glance.campaigns.clear();
            app.data.glance.quiet.count = 0;
            (app.mine.prs, app.mine.sample) = (vec![], crate::prs::Sample::default());
            let repos = app.config.repos.clone();
            app.shipped = Dashboard::for_kind(&repos, Kind::Shipped);
            app.shipped.sources[0].error = Some("GitHub unavailable");
            app.builds.sources.iter_mut().for_each(|s| s.runs.clear());
            app.issues = Dashboard::for_kind(&["example-org/app".into()], Kind::Issues);
            app.issues.sources[0].error = Some("GitHub unavailable");
            app.scope.read_at = None;
            let ids = glance::ids(&p.app.view());
            assert_eq!(ids.len(), 11, "{ids:?}");
            assert!(matches!(ids[0], glance::RowId::Ask(_)));
            assert!(matches!(ids[10], glance::RowId::Health(..)));
            assert!(
                ids[1..10]
                    .iter()
                    .enumerate()
                    .all(|(i, r)| *r == glance::RowId::Card(i + 1))
            );
            let mut seen = vec![];
            for (i, id) in ids.iter().enumerate().skip(1) {
                p.code(KeyCode::Tab);
                assert_eq!(p.app.row, i, "{w}x{h}");
                assert_reachable(&mut p, tiny);
                let screen = p.text();
                let title = glance::card(id);
                // A stop shows its card and state; Health's row says what it reports.
                let line = screen
                    .lines()
                    .nth(hit_rect(&p, i).unwrap().y as usize)
                    .unwrap();
                let want = if i < 10 { title } else { "Health · as of" };
                assert!(
                    line.contains(want) || (!tiny && screen.contains(title)),
                    "{title}:\n{screen}"
                );
                seen.push(title);
                if i < 10 {
                    // An inert stop: nothing answers, opens, parks, copies or goes.
                    for key in ['\n', 'l', 'p', 'y', 'o'] {
                        let effect = p.keys(&key.to_string());
                        assert!(matches!(effect, Effect::None), "{title} {key:?}");
                        assert_eq!(p.app.screen, Screen::Glance, "{title} {key:?}");
                        assert!(p.app.ask.is_none() && p.app.pending.is_none());
                    }
                    p.text();
                    a_lists_asks(&mut p);
                    p.app.status = None;
                    assert!(matches!(glance::selected(&p.app.view()), Selected::Nothing));
                }
            }
            assert_eq!(seen.len(), 10);
            for i in (0..10).rev() {
                p.code(KeyCode::BackTab);
                assert_eq!(p.app.row, i, "{w}x{h}");
                assert_reachable(&mut p, tiny);
            }
            for i in 1..ids.len() {
                p.code(KeyCode::Down);
                assert_eq!(p.app.row, i);
                assert_reachable(&mut p, tiny);
            }
        }
    }

    /// A source that empties under the cursor leaves it on that card's stop, drawn.
    #[test]
    fn a_source_emptying_under_the_cursor_leaves_a_drawn_stop() {
        for (w, h) in [(46, 20), (120, 40)] {
            let mut p = Pane::new(w, h);
            all_on(&mut p);
            let view = p.app.view();
            p.app.row = glance::ids(&view)
                .iter()
                .rposition(|r| matches!(r, glance::RowId::Pr(..)))
                .unwrap();
            assert_reachable(&mut p, false);
            let before = glance::ids(&p.app.view()).get(p.app.row).cloned();
            (p.app.mine.prs, p.app.mine.sample) = (vec![], crate::prs::Sample::default());
            p.app.restore_row(before);
            let ids = glance::ids(&p.app.view());
            assert!(!ids.iter().any(|r| matches!(r, glance::RowId::Pr(..))));
            assert!(matches!(
                ids[p.app.row],
                glance::RowId::Card(2) | glance::RowId::Campaign(_)
            ));
            assert_reachable(&mut p, false);
            assert!(w < 120 || p.text().contains("0 observed in search sample"));
        }
    }

    /// The ↑ and ↓ lines name what is out of view and are shift-tab and tab.
    #[test]
    fn disclosures_name_hidden_cards_and_click_as_tab() {
        let mut p = Pane::new(46, 20);
        all_on(&mut p);
        let screen = p.text();
        let down = screen
            .lines()
            .find(|l| l.contains("↓"))
            .unwrap_or_else(|| panic!("{screen}"));
        assert!(
            down.contains("cards below") && down.contains("HEALTH") || down.ends_with('…'),
            "{down}"
        );
        let starts = glance::sections(&p.app.view());
        p.click_on("↓");
        assert_eq!(p.app.row, starts[1]);
        p.keys("G");
        let screen = p.text();
        let up = screen
            .lines()
            .find(|l| l.contains("↑"))
            .unwrap_or_else(|| panic!("{screen}"));
        assert!(up.contains("above: NEEDS YOU 2"), "{up}");
        assert!(!screen.contains("↓"), "{screen}");
        let last = p.app.row;
        p.click_on("↑");
        assert_eq!(
            p.app.row,
            *starts.iter().rev().find(|&&s| s < last).unwrap()
        );
        // Off the Dashboard a disclosure is not a key: Tab does nothing there.
        let mut asks = Pane::new(40, 12);
        frames::with_structured(&mut asks.app);
        asks.keys("A");
        asks.text();
        assert!(
            !asks
                .app
                .seen
                .borrow()
                .hits
                .iter()
                .any(|(_, h)| *h == Hit::Key("tab".into()))
        );
    }

    /// 80 columns and 20 rows are two columns: asks beside checks; PRs span the width; a band
    /// with one side shown takes the width; nothing pads a short card.
    #[test]
    fn bands_split_at_80_columns_and_a_lone_side_takes_the_width() {
        let mut p = Pane::new(80, 30);
        all_on(&mut p);
        let screen = p.text();
        let a = screen.lines().find(|l| l.contains(" NEEDS YOU ─")).unwrap();
        assert!(a.contains(" TO CHECK ─"), "{a}");
        assert_eq!(
            a.find(" TO CHECK").map(|i| a[..i].chars().count()),
            Some(40),
            "{a}"
        );
        let pr = screen.lines().find(|l| l.contains(" MY PRS ─")).unwrap();
        assert!(
            pr.starts_with(" MY PRS ─") && pr.ends_with("@demo-owner"),
            "{pr}"
        );
        assert_eq!(pr.chars().count(), 79, "{pr}");
        let rect = hit_rect(
            &p,
            glance::ids(&p.app.view())
                .iter()
                .position(|r| matches!(r, glance::RowId::Pr(..)))
                .unwrap(),
        );
        assert_eq!(rect.map(|r| (r.x, r.width)), Some((0, 80)));
        // Checks hidden: the asks take the whole width.
        p.keys("2");
        let screen = p.text();
        let a = screen.lines().find(|l| l.contains(" NEEDS YOU ─")).unwrap();
        assert!(
            !screen.contains(" TO CHECK ─") && a.chars().count() == 79,
            "{a}"
        );
        // Panels 1-3 and 5-0 off, PRs and Health only: the body ends early, no filler.
        let mut p = Pane::new(120, 40);
        all_on(&mut p);
        p.app.config.panels = [
            false, false, false, true, false, false, false, false, false, false, true,
        ];
        let screen = p.text();
        let blank = screen.lines().filter(|l| l.trim().is_empty()).count();
        assert!(blank > 10, "{screen}");
        assert!(screen.lines().last().unwrap().contains("help"), "{screen}");
    }

    /// Campaigns cut by the card: listed ones are counted apart from quiet ones, which
    /// only `c` lists.
    #[test]
    fn the_campaign_overflow_counts_listed_apart_from_quiet() {
        let mut p = Pane::new(46, 20);
        all_on(&mut p);
        p.app.config.panels = [
            true, true, true, false, false, false, false, false, false, false, false,
        ];
        let quiet = p.app.data.glance.quiet.count;
        assert!(quiet > 0);
        let screen = p.text();
        let listed = p.app.data.glance.campaigns.len();
        let rule = screen.lines().find(|l| l.contains(" CAMPAIGNS ─")).unwrap();
        assert!(rule.ends_with(&format!("─ {listed}")), "{rule}");
        let more = screen
            .lines()
            .find(|l| l.contains("more listed"))
            .unwrap_or_else(|| panic!("{screen}"));
        let n: usize = more.trim_start()[1..]
            .split(' ')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!(n < listed, "{more}");
        assert!(
            more.contains(&format!("{quiet} quiet unlisted · c")),
            "{more}"
        );
    }

    /// `I` on an inert stop opens no item: Detail says nothing is selected, no row key acts,
    /// and `h` returns to the same stop. With an item inspected before, Detail keeps that
    /// item, never the stop.
    #[test]
    fn an_inert_stop_is_never_a_detail_item_or_an_action_target() {
        let mut p = Pane::new(46, 20);
        all_on(&mut p);
        p.app.scope.read_at = None;
        let ids = glance::ids(&p.app.view());
        let stop = ids
            .iter()
            .position(|r| *r == glance::RowId::Card(6))
            .expect("Decisions unscoped is a stop");
        p.app.row = stop;
        assert_reachable(&mut p, false);
        p.keys("I");
        assert_eq!(p.app.screen, Screen::Detail);
        assert!(p.app.target.is_none());
        assert!(p.text().contains("Nothing selected yet"), "{}", p.text());
        for key in ['\n', 'l', 'p', 'y', 'o'] {
            let effect = p.keys(&key.to_string());
            assert!(matches!(effect, Effect::None), "{key:?}");
            assert_eq!(p.app.screen, Screen::Detail, "{key:?}");
            assert!(p.app.ask.is_none() && p.app.pending.is_none(), "{key:?}");
        }
        p.text();
        a_lists_asks(&mut p);
        p.keys("h");
        assert_eq!((p.app.screen, p.app.row), (Screen::Glance, stop));
        assert_eq!(
            glance::ids(&p.app.view())[p.app.row],
            glance::RowId::Card(6)
        );
        // Health inspected first: from the stop, `I` shows Health again, not the stop.
        p.keys("G");
        p.keys("I");
        p.keys("h");
        p.app.row = stop;
        p.keys("I");
        let target = p.app.target.as_ref().map(|t| t.id.clone());
        assert!(
            matches!(target, Some(glance::RowId::Health(..))),
            "{target:?}"
        );
        p.keys("h");
        assert_eq!(p.app.row, stop);
    }

    /// A card given its want height (campaigns' second lines cut, nothing else) draws every
    /// row: the line a "+1 more" would take is the row's own.
    #[test]
    fn a_card_at_its_want_height_draws_every_row() {
        for h in 24..=40 {
            let mut p = Pane::new(46, h);
            p.app.config.panels = [
                true, true, true, false, false, false, false, false, false, false, false,
            ];
            p.app.data.glance.quiet.count = 0;
            let names: Vec<String> = p
                .app
                .data
                .glance
                .campaigns
                .iter()
                .map(|c| c.name.clone())
                .collect();
            let text = p.text();
            let drawn = names.iter().filter(|n| text.contains(n.as_str())).count();
            assert!(
                !(drawn + 1 == names.len() && text.contains("+1 more listed")),
                "46x{h}: a +1 line where the row fits:\n{text}"
            );
            if h == 29 {
                // 5 + 7 + 12 lines = the body: all eleven campaigns, no overflow line.
                assert_eq!(drawn, 11, "{text}");
                assert!(!text.contains("more listed"), "{text}");
            }
        }
    }

    /// Notices sit in the rows' gutter: a right column's first cell is blank on every head
    /// line, so it never joins the left card's text, and the notice itself is whole.
    #[test]
    fn right_column_heads_keep_a_blank_gutter() {
        for (w, h) in [(80, 30), (120, 40)] {
            let mut p = Pane::new(w, h);
            all_on(&mut p);
            p.app.shipped.sources[0].total = 100;
            let text = p.text();
            let split = (w as usize).div_ceil(2);
            let mut heads = 0;
            for line in text.lines() {
                let cells: Vec<char> = line.chars().collect();
                let right: String = cells.iter().skip(split).collect();
                if [
                    "sampled; ",
                    "latest100-run sample",
                    "first 100",
                    "Hub report",
                ]
                .iter()
                .any(|m| right.contains(m))
                    && !line.contains(" ─")
                {
                    heads += 1;
                    assert_eq!(cells[split], ' ', "{w}x{h}: {line:?}\n{text}");
                    assert_ne!(cells[split + 1], ' ', "one gutter only: {line:?}");
                }
            }
            assert!(heads >= 3, "{w}x{h}: right-column heads drawn:\n{text}");
            // The capped source (100 sampled, clipped at 80 columns) and the other one whole.
            let capped = if w == 120 {
                "100 sampled; 0s ago · example-org/app"
            } else {
                "100 sampled; 0s ago · "
            };
            for whole in [
                capped,
                "2 sampled; 0s ago · example-org/tools",
                "latest100-run sample",
                "first 100; more may exist",
            ] {
                assert!(text.contains(whole), "{w}x{h}: {whole}\n{text}");
            }
            // An empty card's stop keeps one gutter, now the cursor bar's.
            p.app.builds.sources.iter_mut().for_each(|s| s.runs.clear());
            let stop = glance::ids(&p.app.view())
                .iter()
                .position(|r| *r == glance::RowId::Card(5))
                .expect("an empty Build card is a stop");
            p.app.row = stop;
            let text = p.text();
            let rect = hit_rect(&p, stop).unwrap();
            let line: Vec<char> = text.lines().nth(rect.y as usize).unwrap().chars().collect();
            assert_eq!(line[rect.x as usize], '▌', "{text}");
            assert_ne!(line[rect.x as usize + 1], ' ', "no second gutter: {text}");
        }
    }

    /// Floors that overflow still fill a split band: at 80x30 the campaigns use the lines
    /// beside Shipped and Build instead of leaving them blank, without growing the band.
    #[test]
    fn a_split_band_fills_its_shorter_side_at_the_floors() {
        let mut p = Pane::new(80, 30);
        all_on(&mut p);
        let text = p.text();
        let lines: Vec<Vec<char>> = text.lines().map(|l| l.chars().collect()).collect();
        let split = 40;
        let side = |l: &Vec<char>, right: bool| -> String {
            if right {
                l.iter().skip(split).collect()
            } else {
                l.iter().take(split).collect()
            }
        };
        let start = lines
            .iter()
            .position(|l| {
                side(l, false).starts_with(" CAMPAIGNS ") && side(l, true).starts_with(" SHIPPED ")
            })
            .expect("band C split at 80 columns");
        let run = |right: bool| {
            lines[start..]
                .iter()
                .take_while(|l| !side(l, right).trim().is_empty())
                .count()
        };
        // Shipped (5 lines) and Build (6) stay at their floors; the campaigns take as many.
        assert_eq!((run(false), run(true)), (11, 11), "{text}");
        let campaigns = lines[start + 1..start + 11]
            .iter()
            .filter(|l| {
                side(l, false)
                    .trim_start()
                    .starts_with(['◐', '○', '·', '‖', '•'])
            })
            .count();
        assert!(campaigns >= 8, "{campaigns} campaigns:\n{text}");
        assert!(
            text.contains("more listed"),
            "the rest still counted: {text}"
        );
        // The viewport below is unchanged: the same cards are disclosed.
        assert!(text.contains("↓ 5 cards below: DECISIONS"), "{text}");
        // Every campaign drawn there is reachable and only drawn rows take clicks.
        let ids = glance::ids(&p.app.view());
        for (i, id) in ids.iter().enumerate() {
            if matches!(id, glance::RowId::Campaign(_)) && hit_rect(&p, i).is_some() {
                p.app.row = i;
                assert_reachable(&mut p, false);
            }
        }
    }

    /// Twelve asks walked down and back with j and k at the narrow heights: each selected
    /// ask is drawn in the window, the rule counts it, the overflow line counts the rest.
    #[test]
    fn twelve_asks_walk_down_and_back_in_the_window() {
        for h in [20, 30] {
            let mut p = Pane::new(46, h);
            let mut next = p.app.data.glance.clone();
            let base = next.needs_you[0].clone();
            next.needs_you = (0..12)
                .map(|i| crate::model::Need {
                    ask_id: 80_000 + i,
                    campaign: format!("ask-lane-{i:02}"),
                    ..base.clone()
                })
                .collect();
            // As a read brings them: the cursor stays at the top, on the first new ask.
            p.app.snapshot(next);
            let mut order: Vec<i64> = (0..12).collect();
            order.extend((0..11).rev());
            let mut last = None;
            for (step, i) in order.into_iter().enumerate() {
                if let Some(prev) = last {
                    p.keys(if i > prev { "j" } else { "k" });
                }
                last = Some(i);
                let id = match glance::selected(&p.app.view()) {
                    Selected::Ask(a) => a.ask_id,
                    _ => panic!("46x{h} step {step}: not an ask"),
                };
                assert_eq!(id, 80_000 + i, "46x{h} step {step}");
                assert_reachable(&mut p, false);
                let text = p.text();
                let name = format!("ask-lane-{i:02}");
                assert!(
                    text.lines()
                        .any(|l| l.starts_with('▌') && l.contains(&name)),
                    "46x{h} step {step}: {name} not drawn selected\n{text}"
                );
                let drawn = text.lines().filter(|l| l.contains("? ◇ ask-lane-")).count();
                assert!((1..12).contains(&drawn), "46x{h}: {drawn}\n{text}");
                let rule = text.lines().find(|l| l.contains(" NEEDS YOU ")).unwrap();
                assert!(rule.ends_with(&format!(" {}/12", i + 1)), "{rule}");
                assert!(
                    text.contains(&format!("+{} more asks · j k", 12 - drawn)),
                    "46x{h} step {step}:\n{text}"
                );
                assert!(text.lines().next().unwrap().contains("12 need you"));
                assert!(text.contains("A Asks 12"), "{text}");
            }
        }
    }

    /// `a` off an ask, on the frame last drawn: the picker and nothing else (no answer, no
    /// pending write, no effect), and esc gives the exact origin back. Over a pane too small
    /// for anything but the pill, `a` does nothing at all.
    fn a_lists_asks(p: &mut Pane) {
        let before = origin(p);
        let target = p.app.target.as_ref().map(|t| t.id.clone());
        let drawn = !p.app.seen.borrow().hits.is_empty();
        assert_eq!(press(&mut p.app, KeyCode::Char('a')), Effect::None);
        assert!(p.app.ask.is_none() && p.app.pending.is_none());
        if !drawn {
            assert_eq!(origin(p), before);
            return;
        }
        assert_eq!(p.app.screen, Screen::Picker);
        assert_eq!(press(&mut p.app, KeyCode::Esc), Effect::None);
        assert_eq!(origin(p), before);
        assert_eq!(p.app.target.as_ref().map(|t| t.id.clone()), target);
        p.app.status = None;
    }

    /// Clicks the first cell of what the last frame recorded as `hit`.
    fn click_hit(p: &mut Pane, hit: Hit) -> Effect {
        p.text();
        let area = p
            .app
            .seen
            .borrow()
            .hits
            .iter()
            .find(|(_, h)| *h == hit)
            .map(|(r, _)| *r)
            .unwrap_or_else(|| panic!("{hit:?} is not drawn"));
        p.click(area.x, area.y)
    }

    /// Where a screen is: what the picker must give back untouched.
    fn origin(p: &Pane) -> (Screen, Option<glance::RowId>, usize, String, usize, usize) {
        let a = &p.app;
        let at = a.screen.rows().then(|| at_row(p)).flatten();
        (
            a.screen,
            at,
            a.row,
            a.filter.clone(),
            a.scroll,
            a.back.len(),
        )
    }

    #[test]
    fn a_off_an_ask_opens_the_picker_and_esc_returns_to_the_exact_origin() {
        for keys in [
            "G", "/docs\nG", "Fjj", "Fjj/o\n", "GI", "D?", "Dc", "Ds", "Dj\t",
        ] {
            for close in ["\x1b", "q"] {
                let mut p = Pane::new(120, 40);
                p.app.config.panels = [true; 11];
                p.keys(keys);
                let before = origin(&p);
                let target = p.app.target.as_ref().map(|t| t.id.clone());
                assert_eq!(p.keys("a"), Effect::None, "{keys}");
                assert_eq!(p.app.screen, Screen::Picker, "{keys}");
                // A modal: no source reads under it; the ledger snapshot is not a source.
                assert!((0..11).all(|i| !p.app.admits(i)), "{keys}");
                assert!(p.text().contains("open owner asks"), "{keys}");
                p.keys(close);
                assert_eq!(origin(&p), before, "{keys} then {close:?}");
                assert_eq!(p.app.target.as_ref().map(|t| t.id.clone()), target);
            }
        }
        // On an ask row `a` goes straight to the editor, as before.
        let mut p = Pane::new(120, 40);
        p.keys("a");
        assert_eq!(p.app.screen, Screen::Answer);
    }

    #[test]
    fn picker_a_shows_the_asks_screen_once_and_the_origin_keeps_its_place() {
        let mut p = Pane::new(120, 40);
        let second = p.app.data.glance.needs_you[1].ask_id;
        // The Asks screen's own filter would hide the ask: it gives way.
        p.keys("A/zzz\nFjj");
        let fleet = at_row(&p);
        p.keys("aj");
        assert_eq!(p.app.picked().map(|a| a.ask_id), Some(second));
        p.keys("A");
        assert_eq!((p.app.screen, p.app.back.len()), (Screen::Asks, 0));
        assert_eq!(at_row(&p), Some(glance::RowId::Ask(second)));
        assert_eq!(p.app.filter, "");
        // One transition: on the Asks screen `A` stays there.
        p.keys("A");
        assert_eq!(at_row(&p), Some(glance::RowId::Ask(second)));
        p.keys("F");
        assert_eq!((p.app.screen, at_row(&p)), (Screen::Fleet, fleet.clone()));
        // With nothing open: the picker says so, and A and esc still work.
        p.app.data.glance.needs_you.clear();
        p.keys("a");
        assert!(p.text().contains("Nothing needs you"), "{}", p.text());
        assert_eq!(p.keys("\n"), Effect::None);
        assert_eq!(p.app.screen, Screen::Picker);
        p.keys("A");
        assert_eq!(p.app.screen, Screen::Asks);
        p.keys("Fa\x1b");
        assert_eq!((p.app.screen, at_row(&p)), (Screen::Fleet, fleet));
    }

    #[test]
    fn picker_enter_answers_the_highlighted_ask_and_esc_returns_to_the_origin() {
        let mut p = structured(120, 40);
        p.keys("Fjj");
        let before = origin(&p);
        let ask = p.app.data.glance.needs_you[1].clone();
        p.keys("aj\n");
        assert_eq!(p.app.screen, Screen::Answer);
        assert_eq!(p.app.ask.as_ref().map(|a| a.ask_id), Some(ask.ask_id));
        // The picker's place went to the editor: esc is back on Fleet, nothing sent.
        assert_eq!(p.keys("\x1b"), Effect::None);
        assert_eq!(origin(&p), before);
        assert_eq!(p.status(), "cancelled, nothing was sent");
        // The picker owns every key: screen keys, brackets and row keys do nothing.
        p.keys("a");
        for c in "DFI[]?s/ycplo0H".chars() {
            assert_eq!(p.code(KeyCode::Char(c)), Effect::None, "{c}");
            assert_eq!(p.app.screen, Screen::Picker, "{c}");
        }
        // Wheel and click choose; a double-click answers; nothing is sent.
        p.mouse(MouseEventKind::ScrollDown, 60, 20);
        assert_eq!(p.app.picked().map(|a| a.ask_id), Some(ask.ask_id));
        let third = p.app.data.glance.needs_you[2].clone();
        assert_eq!(click_hit(&mut p, Hit::Ask(third.ask_id)), Effect::None);
        assert_eq!(p.app.picked().map(|a| a.ask_id), Some(third.ask_id));
        // A click outside the picker lands on nothing.
        assert_eq!(p.click(0, 39), Effect::None);
        assert_eq!(p.app.screen, Screen::Picker);
        assert_eq!(click_hit(&mut p, Hit::Ask(third.ask_id)), Effect::None);
        assert_eq!(p.app.screen, Screen::Answer);
        assert_eq!(p.app.ask.as_ref().map(|a| a.ask_id), Some(third.ask_id));
    }

    #[test]
    fn the_picker_follows_its_ask_across_snapshots_and_says_when_it_leaves() {
        let mut p = structured(120, 40);
        p.keys("Faj");
        let id = p.app.picked().map(|a| a.ask_id).unwrap();
        let mut next = p.app.data.glance.clone();
        next.needs_you.reverse();
        p.app.snapshot(next.clone());
        assert_eq!(p.app.picked().map(|a| a.ask_id), Some(id));
        assert_eq!(p.app.screen, Screen::Picker);
        next.needs_you.retain(|a| a.ask_id != id);
        p.app.snapshot(next.clone());
        assert!(p.status().contains("is no longer open"), "{}", p.status());
        assert_eq!(
            p.app.picked().map(|a| a.ask_id),
            Some(next.needs_you[0].ask_id)
        );
        p.keys("\n");
        assert_eq!(
            p.app.ask.as_ref().map(|a| a.ask_id),
            Some(next.needs_you[0].ask_id)
        );
    }

    #[test]
    fn every_printable_is_text_in_the_editor_and_options_only_choose() {
        let mut p = structured(120, 40);
        p.keys("ja");
        assert_eq!(p.app.screen, Screen::Answer);
        let options = ask::parse(p.app.ask.as_ref().unwrap()).options.len();
        // An option click chooses it, a second click too; neither sends nor reviews.
        assert_eq!(click_hit(&mut p, Hit::Opt(2)), Effect::None);
        assert_eq!(click_hit(&mut p, Hit::Opt(2)), Effect::None);
        assert_eq!((p.app.screen, p.app.choice), (Screen::Answer, 2));
        // The wheel scrolls the ask's text and never moves the choice.
        p.mouse(MouseEventKind::ScrollDown, 60, 20);
        p.mouse(MouseEventKind::ScrollUp, 60, 20);
        assert_eq!(p.app.choice, 2);
        // Nothing outside the dialog can be clicked.
        assert_eq!(p.click(0, 39), Effect::None);
        assert_eq!((p.app.screen, p.app.choice), (Screen::Answer, 2));
        // "write your own answer": every printable is text, a first bracket included.
        click_hit(&mut p, Hit::Opt(options));
        assert_eq!((p.app.choice, p.app.editing), (options, true));
        let typed = "[]DAFIAa?qjk y/s0H";
        p.keys(typed);
        assert_eq!((p.app.screen, p.app.text.as_str()), (Screen::Answer, typed));
    }

    #[test]
    fn a_failed_read_never_closes_the_editor_and_a_good_one_does() {
        use crate::client::{Live, Pace, Update};
        let (tx, rx) = std::sync::mpsc::channel();
        let mut live = Live::new(rx, Pace::default(), None);
        let mut p = Pane::new(120, 40);
        p.keys("a");
        let id = p.app.ask.as_ref().unwrap().ask_id;
        tx.send(Update::Started(Instant::now())).unwrap();
        tx.send(Update::Done(Box::new(Err("timeout".into()))))
            .unwrap();
        live.poll(&mut p.app, Instant::now());
        assert!(!p.app.gone && !p.text().contains("no longer open"));
        let mut next = p.app.data.glance.clone();
        next.needs_you.retain(|a| a.ask_id != id);
        tx.send(Update::Started(Instant::now())).unwrap();
        tx.send(Update::Done(Box::new(Ok(next)))).unwrap();
        live.poll(&mut p.app, Instant::now());
        assert!(p.app.gone && p.text().contains("no longer open"));
        // Another failure does not reopen it.
        tx.send(Update::Done(Box::new(Err("timeout".into()))))
            .unwrap();
        live.poll(&mut p.app, Instant::now());
        assert!(p.app.gone);
    }

    #[test]
    fn help_open_before_the_first_snapshot_returns_to_the_first_ask() {
        let mut p = Pane::new(120, 40);
        let glance = p.app.data.glance.clone();
        p.app.data = crate::model::Data::default();
        p.app.fetch.loaded = false;
        p.app.mine = crate::prs::demo(p.app.observed_at);
        // The owner moved onto the PR card and opened the help, then the ledger answered.
        p.keys("jj?");
        assert_eq!(p.app.screen, Screen::Help);
        p.app.snapshot(glance.clone());
        p.app.fetch.loaded = true;
        p.keys("\x1b");
        assert_eq!(
            (p.app.screen, at_row(&p)),
            (
                Screen::Glance,
                Some(glance::RowId::Ask(glance.needs_you[0].ask_id))
            )
        );
    }

    #[test]
    fn the_asks_screen_shows_the_selected_ask_in_full() {
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        // Wide: beside the list, scrolled on its own; j k move between asks. (At 120x40 the
        // long ask fits beside the list; 100x24 makes it scroll.)
        let mut p = Pane::new(100, 24);
        p.app.data.glance.needs_you.insert(0, frames::long_ask());
        let long = p.app.data.glance.needs_you[0].ask_id;
        p.keys("Ag");
        let text = p.text();
        assert!(text.contains(&format!("ask {long} ·")), "{text}");
        assert!(text.contains("ctrl-d ctrl-u scroll text"), "{text}");
        key(&mut p.app, ctrl('d'));
        assert!(p.app.scroll > 0);
        assert_eq!(at_row(&p), Some(glance::RowId::Ask(long)));
        p.keys("j");
        assert_eq!(p.app.scroll, 0);
        // The place is kept across screens.
        p.keys("k");
        key(&mut p.app, ctrl('d'));
        let scroll = p.app.scroll;
        p.keys("DA");
        assert_eq!(
            (p.app.scroll, at_row(&p)),
            (scroll, Some(glance::RowId::Ask(long)))
        );
        // Narrow with several asks: the list, and ⏎ reads the selected one on Detail.
        let mut p = Pane::new(46, 30);
        p.keys("Aj");
        let id = at_row(&p);
        assert!(p.text().contains("⏎ read in full"), "{}", p.text());
        assert_eq!(p.keys("\n"), Effect::None);
        assert_eq!(p.app.screen, Screen::Detail);
        assert_eq!(p.app.target.as_ref().map(|t| t.id.clone()), id);
        p.keys("\x1b");
        assert_eq!((p.app.screen, at_row(&p)), (Screen::Asks, id));
        // Narrow with one: its full text under it, and ⏎ answers it.
        let mut p = Pane::new(46, 30);
        p.app.data.glance.needs_you.truncate(1);
        let one = p.app.data.glance.needs_you[0].clone();
        p.keys("A");
        let text = p.text();
        assert!(text.contains(&format!("ask {} ·", one.ask_id)), "{text}");
        assert!(text.contains("⏎ answer"), "{text}");
        assert_eq!(p.keys("\n"), Effect::None);
        assert_eq!(p.app.screen, Screen::Answer);
        assert_eq!(p.app.ask.as_ref().map(|a| a.ask_id), Some(one.ask_id));
    }

    #[test]
    fn enter_answers_the_ask_the_asks_screen_shows_in_full_in_an_80x24_dialog() {
        for (width, height) in [(120, 40), (80, 30)] {
            let mut p = Pane::new(width, height);
            p.app.data.glance.needs_you.insert(0, frames::long_ask());
            let long = p.app.data.glance.needs_you[0].ask_id;
            p.keys("Ag");
            let text = p.text();
            assert!(
                text.contains("⏎ answer") && !text.contains("⏎ go"),
                "{text}"
            );
            // ⏎ opens the editor on that ask, never its pane, and sends nothing.
            assert_eq!(p.keys("\n"), Effect::None);
            assert_eq!(p.app.screen, Screen::Answer);
            assert_eq!(p.app.ask.as_ref().map(|a| a.ask_id), Some(long));
            assert!(p.app.pending.is_none());
            // The dialog is 24 rows, centred: its title on its top border.
            let text = p.text();
            let title = text.lines().position(|l| l.contains("Answer ask")).unwrap();
            assert_eq!(title, (height as usize - 24) / 2, "{text}");
            // The wheel scrolls the long ask's text, never the choice.
            let choice = p.app.choice;
            p.mouse(MouseEventKind::ScrollDown, width / 2, height / 2);
            p.mouse(MouseEventKind::ScrollDown, width / 2, height / 2);
            assert_eq!((p.app.scroll, p.app.choice), (2, choice));
            p.mouse(MouseEventKind::ScrollUp, width / 2, height / 2);
            assert_eq!((p.app.scroll, p.app.choice), (1, choice));
            // esc gives the Asks screen back on the same ask.
            p.keys("\x1b");
            assert_eq!(
                (p.app.screen, at_row(&p)),
                (Screen::Asks, Some(glance::RowId::Ask(long)))
            );
        }
        // A smaller pane shrinks the dialog to fit; under 60 columns it is full screen.
        let mut p = Pane::new(70, 20);
        p.keys("Aa");
        assert_eq!(p.app.screen, Screen::Answer);
        assert!(p.text().contains("Answer ask"), "{}", p.text());
    }

    #[test]
    fn every_picker_hint_does_what_it_says() {
        // Narrow: the picker is full screen with its hints in the footer.
        let mut p = structured(46, 30);
        p.keys("Fa");
        let text = p.text();
        for hint in ["j k choose", "⏎ answer", "A Asks", "esc close"] {
            assert!(text.contains(hint), "{hint}\n{text}");
        }
        p.click_on("j k choose");
        assert_eq!(
            p.app.picked().map(|a| a.ask_id),
            Some(p.app.data.glance.needs_you[1].ask_id)
        );
        p.click_on("⏎ answer");
        assert_eq!(p.app.screen, Screen::Answer);
        p.keys("\x1ba");
        p.click_on("A Asks");
        assert_eq!(p.app.screen, Screen::Asks);
        p.keys("Fa");
        p.click_on("esc close");
        assert_eq!(p.app.screen, Screen::Fleet);
    }

    /// Puts the Fleet cursor on campaign `id`.
    fn fleet_at(p: &mut Pane, id: i64) {
        p.app.row = glance::ids(&p.app.view())
            .iter()
            .position(|r| *r == glance::RowId::Campaign(id))
            .unwrap();
    }

    /// The screen line the cursor's row is drawn on, when it is drawn as a hit target.
    fn cursor_line(p: &mut Pane) -> Option<String> {
        let text = p.text();
        let y = p
            .app
            .seen
            .borrow()
            .hits
            .iter()
            .find(|(_, h)| *h == Hit::Row(p.app.row))
            .map(|(r, _)| r.y)?;
        text.lines().nth(y as usize).map(str::to_string)
    }

    #[test]
    fn fleet_reaches_every_listed_campaign_at_every_size() {
        for (w, h) in [(120, 40), (80, 30), (46, 30), (46, 20), (40, 12), (30, 10)] {
            let mut p = Pane::new(w, h);
            p.keys("F");
            let ids = glance::ids(&p.app.view());
            assert_eq!(ids.len(), p.app.data.glance.campaigns.len(), "{w}x{h}");
            // Active first, the parked one last.
            assert_eq!(ids.last(), Some(&glance::RowId::Campaign(3254)));
            p.keys("g");
            for (i, id) in ids.iter().enumerate() {
                assert_eq!(p.app.row, i, "{w}x{h}");
                let name = p
                    .app
                    .data
                    .glance
                    .campaigns
                    .iter()
                    .find(|c| glance::RowId::Campaign(c.id) == *id)
                    .unwrap()
                    .name
                    .clone();
                let Some(line) = cursor_line(&mut p) else {
                    panic!("{w}x{h}: row {i} is not drawn:\n{}", p.text());
                };
                assert!(
                    line.starts_with('▌') && line.contains(&name[..name.len().min(8)]),
                    "{w}x{h}: {line}"
                );
                // Listed only: the totals stay on screen, whatever the cursor's window.
                assert!(p.text().contains("listed"), "{w}x{h}:\n{}", p.text());
                assert!(!matches!(
                    press(&mut p.app, KeyCode::Char('j')),
                    Effect::Run(_) | Effect::Refresh
                ));
            }
            assert_eq!(p.app.row, ids.len() - 1, "{w}x{h}");
        }
    }

    #[test]
    fn fleet_keeps_its_row_through_reorder_park_removal_and_filter() {
        let mut p = Pane::new(120, 40);
        p.keys("F");
        fleet_at(&mut p, 4120);
        let mut next = p.app.data.glance.clone();
        next.campaigns.reverse();
        p.app.snapshot(next);
        assert_eq!(at_row(&p), Some(glance::RowId::Campaign(4120)));
        assert!(cursor_line(&mut p).unwrap().contains("tui-frames"));
        // Parked, it moves among the parked ones, and the cursor with it.
        let mut next = p.app.data.glance.clone();
        next.campaigns
            .iter_mut()
            .find(|c| c.id == 4120)
            .unwrap()
            .parked = true;
        p.app.snapshot(next);
        assert_eq!(at_row(&p), Some(glance::RowId::Campaign(4120)));
        let line = cursor_line(&mut p).unwrap();
        assert!(
            line.contains("tui-frames") && line.contains("parked"),
            "{line}"
        );
        let ids = glance::ids(&p.app.view());
        let first_parked = ids
            .iter()
            .position(|r| *r == glance::RowId::Campaign(3254))
            .unwrap()
            .min(p.app.row);
        assert!(
            ids[first_parked..].iter().all(|r| p
                .app
                .data
                .glance
                .campaigns
                .iter()
                .any(|c| glance::RowId::Campaign(c.id) == *r && c.parked)),
            "parked last: {ids:?}"
        );
        let mut next = p.app.data.glance.clone();
        next.campaigns
            .iter_mut()
            .find(|c| c.id == 4120)
            .unwrap()
            .parked = false;
        p.app.snapshot(next);
        assert_eq!(at_row(&p), Some(glance::RowId::Campaign(4120)));
        // Removed: the cursor lands on a drawn row, never on nothing.
        let mut next = p.app.data.glance.clone();
        next.campaigns.retain(|c| c.id != 4120);
        p.app.snapshot(next);
        assert!(matches!(at_row(&p), Some(glance::RowId::Campaign(_))));
        assert!(cursor_line(&mut p).is_some());
        // Filtered: the matching subset, its totals named as such.
        p.keys("/doc\n");
        assert_eq!(at_row(&p), Some(glance::RowId::Campaign(4296)));
        let text = p.text();
        assert!(
            text.contains("in 1 of 10 listed campaigns matching \"doc\" (listed only"),
            "{text}"
        );
    }

    #[test]
    fn fleet_shows_a_missing_count_as_unknown_never_as_zero() {
        for (w, row, total) in [
            (
                120,
                "1/?/1",
                "working 13 · ready 5+? · open 22 lanes in 11 listed campaigns",
            ),
            (46, "1/?/1", "13/5+?/22 lanes w/r/o"),
        ] {
            let mut p = Pane::new(w, 30);
            p.app.data.glance.campaigns[0].lanes.known = [true, false, true];
            p.keys("F");
            let text = p.text();
            let line = text.lines().find(|l| l.contains("search-index")).unwrap();
            assert!(line.contains(row), "{w}: {line}");
            assert!(
                text.contains(total) && text.contains("listed only"),
                "{w}:\n{text}"
            );
        }
        let mut p = Pane::new(120, 40);
        p.app.data.glance.campaigns[0].lanes.known = [true, false, true];
        p.keys("F");
        assert!(
            p.text()
                .contains("(listed only, not every host) · ? not reported")
        );
    }

    #[test]
    fn fleet_is_its_own_table_and_the_dashboard_keeps_its_card() {
        let mut p = Pane::new(120, 40);
        // Panel 3 hidden on the Dashboard: Fleet still lists every campaign.
        p.keys("3F");
        let text = p.text();
        for column in [
            "lead state",
            "campaign",
            "host",
            "lanes w/r/o",
            "activity 4h",
            "last milestone",
            "age",
        ] {
            assert!(text.contains(column), "{column}:\n{text}");
        }
        assert!(text.contains("quiet 2: cache-audit · font-swap"), "{text}");
        let at = |name: &str| text.lines().position(|l| l.contains(name)).unwrap();
        assert!(
            at("api-pagination") < at("legacy-importer") && at("legacy-importer") < at("quiet 2")
        );
        // The owner note leads, `OWNER: nothing.` stripped; a note of only that gives way.
        let tui = text.lines().find(|l| l.contains("tui-frames")).unwrap();
        assert!(
            tui.contains("NOW: plan rev 3.1") && !tui.contains("OWNER"),
            "{tui}"
        );
        let api = text.lines().find(|l| l.contains("api-pagination")).unwrap();
        assert!(
            api.contains("S4 cursor split merged") && !api.contains("OWNER"),
            "{api}"
        );
        // Before any deliberate read the lanes say how to get them.
        assert!(
            text.contains("nothing read yet") && text.contains("l opens the selected campaign")
        );
        p.keys("D3");
        let text = p.text();
        assert!(
            text.contains("CAMPAIGNS") && !text.contains("lead state") && !text.contains("LANES"),
            "{text}"
        );
    }

    #[test]
    fn fleet_with_no_listed_campaign_claims_no_idle_fleet() {
        let mut p = Pane::new(80, 30);
        p.app.data.glance.campaigns.clear();
        p.keys("F");
        let text = p.text();
        assert!(
            text.contains("No listed campaigns; not a fleet-wide idle claim")
                && text.contains("quiet 2"),
            "{text}"
        );
        for code in [KeyCode::Enter, KeyCode::Char('p'), KeyCode::Char('l')] {
            assert_eq!(p.code(code), Effect::None);
        }
        assert!(p.app.pending.is_none() && p.app.screen == Screen::Fleet);
    }

    #[test]
    fn fleet_lanes_show_the_last_explicit_read_and_never_follow_the_cursor() {
        let mut p = Pane::new(120, 40);
        p.keys("F");
        // Moving the cursor reads nothing.
        for key in ['j', 'j', 'k', 'G', 'g'] {
            p.text();
            assert_eq!(press(&mut p.app, KeyCode::Char(key)), Effect::None);
        }
        // `l` on tui-frames reads it and opens the campaign; esc returns to its row.
        fleet_at(&mut p, 4120);
        p.keys("l");
        assert_eq!(p.app.screen, Screen::Campaign);
        p.keys("\x1b");
        assert_eq!(
            (p.app.screen, at_row(&p)),
            (Screen::Fleet, Some(glance::RowId::Campaign(4120)))
        );
        let text = p.text();
        assert!(
            text.contains("LANES · tui-frames #4120")
                && text.contains("read 0s ago")
                && text.contains("impl-frames")
                && text.contains(" report")
                && text.contains("6 of 29 lanes, open shown")
                && text.contains("read 0s ago · l re-reads")
                && text.contains("l opens all"),
            "{text}"
        );
        // The cursor on another campaign: the lanes stay tui-frames', nothing is read, and
        // no hint promises that `l` here re-reads them.
        p.text();
        assert_eq!(press(&mut p.app, KeyCode::Char('g')), Effect::None);
        assert_eq!(at_row(&p), Some(glance::RowId::Campaign(4310)));
        let text = p.text();
        assert!(
            text.contains("LANES · tui-frames #4120")
                && !text.contains("l re-reads")
                && !text.contains("l opens all"),
            "{text}"
        );
        // `l` there: until its read lands, the lanes on hand stay, by their own name.
        p.text();
        let Effect::Run(job) = press(&mut p.app, KeyCode::Char('l')) else {
            panic!("l reads the campaign")
        };
        assert!(matches!(
            job,
            Job::Campaign {
                root: 4310,
                open: true,
                ..
            }
        ));
        let text = p.text();
        assert!(
            text.contains("reading search-index…") && text.contains("LANES · tui-frames #4120"),
            "{text}"
        );
        // The ledger sends another campaign: refused; the cached identity stays.
        let wrong = p.app.data.campaign.clone();
        done(&mut p.app, &job, Ok(Done::Campaign(Box::new(wrong))));
        let text = p.text();
        assert!(
            text.contains("LANES · tui-frames #4120")
                && text.contains("the ledger sent campaign 4120 · showing the last good read")
                && !text.contains("reading search-index…"),
            "{text}"
        );
        // A late reply to an older read is dropped: nothing retargets.
        let late = Job::Campaign {
            root: 4310,
            name: "search-index".into(),
            open: true,
            seq: 0,
        };
        let mut other = p.app.data.campaign.clone();
        (other.root.id, other.root.name) = (4310, "search-index".into());
        done(&mut p.app, &late, Ok(Done::Campaign(Box::new(other))));
        assert_eq!(p.app.data.campaign.root.id, 4120);
        assert!(p.text().contains("LANES · tui-frames #4120"));
        // A failed re-read keeps the last good read and its age.
        fleet_at(&mut p, 4120);
        p.text();
        let Effect::Run(job) = press(&mut p.app, KeyCode::Char('l')) else {
            panic!("l re-reads the campaign")
        };
        done(&mut p.app, &job, Err("hub unreachable".into()));
        p.keys("\x1b");
        p.app.observed_at += 90;
        let text = p.text();
        assert!(
            text.contains("LANES · tui-frames #4120")
                && text.contains("read 1m ago")
                && text.contains(
                    "reading tui-frames failed: hub unreachable · showing the last good read"
                ),
            "{text}"
        );
        assert_eq!(at_row(&p), Some(glance::RowId::Campaign(4120)));
    }

    #[test]
    fn short_fleet_keeps_one_honest_line_for_the_lanes() {
        let mut p = Pane::new(40, 12);
        p.keys("F");
        assert!(p.text().contains("l opens a campaign"), "{}", p.text());
        fleet_at(&mut p, 4120);
        p.keys("l\x1b");
        assert_eq!(p.app.screen, Screen::Fleet);
        let text = p.text();
        assert!(text.contains("LANES tui-frames #4120"), "{text}");
        assert!(cursor_line(&mut p).unwrap().contains("tui-fram"));
    }

    #[test]
    fn fleet_detail_says_what_a_narrow_row_drops() {
        let mut p = Pane::new(46, 30);
        let c = p
            .app
            .data
            .glance
            .campaigns
            .iter_mut()
            .find(|c| c.id == 4087)
            .unwrap();
        c.lanes.known = [true, false, true];
        p.keys("F");
        fleet_at(&mut p, 4087);
        p.keys("I");
        let text = p.text();
        assert!(
            text.contains("ios-widgets")
                && text.contains("laptop")
                && text.contains("0/?/0")
                && !text.contains("lanes —"),
            "{text}"
        );
    }

    #[test]
    fn fleet_lanes_keep_root_and_age_for_a_long_name() {
        for (w, h) in [(46, 30), (40, 12)] {
            let mut p = Pane::new(w, h);
            p.keys("F");
            fleet_at(&mut p, 4120);
            p.keys("l\x1b");
            let long = "warehouse-inventory-reconciliation-tui";
            p.app.data.campaign.root.name = long.into();
            let c = p
                .app
                .data
                .glance
                .campaigns
                .iter_mut()
                .find(|c| c.id == 4120)
                .unwrap();
            c.name = long.into();
            let text = p.text();
            assert!(
                text.contains("read 0s ago")
                    && text.contains("#4120")
                    && text.contains("warehouse"),
                "{w}x{h}:\n{text}"
            );
        }
    }

    #[test]
    fn fleet_lanes_hang_an_open_lane_under_a_closed_one_from_the_lead() {
        let mut p = Pane::new(120, 40);
        p.keys("F");
        fleet_at(&mut p, 4120);
        p.keys("l\x1b");
        p.app
            .data
            .campaign
            .lanes
            .iter_mut()
            .find(|l| l.id == 4190)
            .unwrap()
            .status = "closed".into();
        let text = p.text();
        assert!(
            text.contains("rev-list-harness")
                && text.contains("impl-list-harness")
                && !text.contains("sub-list-core")
                && text.contains("5 of 29 lanes, open shown"),
            "{text}"
        );
    }

    #[test]
    fn fleet_rows_and_hints_answer_the_mouse() {
        let mut p = Pane::new(120, 40);
        p.keys("F");
        p.click_on("model-evals");
        assert_eq!(at_row(&p), Some(glance::RowId::Campaign(4215)));
        // A double-click is ⏎: the lead's pane, on this machine.
        assert!(matches!(
            p.click_on("model-evals"),
            Effect::Run(Job::Focus { .. })
        ));
        // The column names, the quiet line, the totals and the lanes are not rows.
        for text in ["lead state", "quiet 2", "totals", "nothing read yet"] {
            p.click_on(text);
            assert_eq!(at_row(&p), Some(glance::RowId::Campaign(4215)), "{text}");
        }
        p.click_on("c closed too");
        assert_eq!(p.app.screen, Screen::AllCampaigns);
        p.keys("\x1b");
        assert_eq!(
            (p.app.screen, at_row(&p)),
            (Screen::Fleet, Some(glance::RowId::Campaign(4215)))
        );
    }

    #[test]
    fn fleet_detail_and_asks_return_to_the_same_row() {
        let mut p = Pane::new(80, 30);
        p.keys("F");
        fleet_at(&mut p, 4215);
        p.keys("I");
        assert!(p.text().contains("from Fleet · CAMPAIGNS"), "{}", p.text());
        p.keys("\x1b");
        assert_eq!(
            (p.app.screen, at_row(&p)),
            (Screen::Fleet, Some(glance::RowId::Campaign(4215)))
        );
        p.keys("a");
        assert_eq!(p.app.screen, Screen::Picker);
        p.keys("\x1b");
        assert_eq!(
            (p.app.screen, at_row(&p)),
            (Screen::Fleet, Some(glance::RowId::Campaign(4215)))
        );
        assert!(cursor_line(&mut p).unwrap().contains("model-evals"));
    }
}
