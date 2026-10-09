//! taskr-tui: the owner's terminal view of the taskr ledger (plan §2 and §3).
//!
//! Every screen draws from a [`model::Data`] value; [`frames`] renders them from a fixture
//! as golden frames. [`client`] reads the ledger in the background, [`input`] turns keys
//! and the mouse into state changes and [`actions`] jobs, and `main` runs the loop.

pub mod actions;
pub mod client;
pub mod frames;
pub mod input;
pub mod model;
pub mod theme;

mod ask;
mod campaign;
mod dialog;
mod glance;
mod keys;
mod lists;
mod slotr;
mod ui;

use std::{cell::RefCell, rc::Rc, time::Instant};

use ratatui::{
    Frame, Terminal, backend::Backend, buffer::CellDiffOption, layout::Rect, text::Line,
    widgets::Paragraph,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Screen {
    #[default]
    Glance,
    Campaign,
    /// The answer dialog, step 1: choose an option or write.
    Answer,
    /// Step 2: confirm the exact text.
    Confirm,
    Help,
    /// Every root, closed ones too.
    AllCampaigns,
    Pager,
    /// slotr's pools: holders and the queue (`s`).
    Slotr,
}

/// How fresh the snapshot is. The view's own state, not part of the snapshot.
#[derive(Debug, Clone, Default)]
pub struct Fetch {
    /// A snapshot has arrived at least once. Until then the glance shows "reading the
    /// ledger", or "no data" when the fetch failed.
    pub loaded: bool,
    /// Age of the snapshot on screen.
    pub age_ms: i64,
    /// Why the last fetch failed; the frame is stale while this is set.
    pub error: Option<String>,
    /// A fetch has been in flight for more than 300 ms: the header shows the spinner.
    pub in_flight: bool,
    /// Spinner frame.
    pub tick: usize,
    pub retry_in_s: u32,
    /// Failed fetches in a row.
    pub tries: u32,
    /// How the view learns of changes: `Some(true)` pushed by the hub, `Some(false)`
    /// polling, `None` when nothing is fetched (the demo, a frame).
    pub live: Option<bool>,
    /// Why it is polling, when the subscription said.
    pub why: String,
}

/// Something the pointer can land on, recorded while drawing so a click needs no second
/// copy of the layout.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Hit {
    /// A row of the glance or of the all-campaigns list.
    Row(usize),
    Lane(usize),
    Pane(usize),
    /// The glance's detail pane, drawn only in the wide layout.
    Detail,
    /// A footer hint: the same as its key.
    Key(String),
}

/// What the last frame showed, for the next key or click.
#[derive(Debug, Default)]
pub(crate) struct Seen {
    pub hits: Vec<(Rect, Hit)>,
    /// Rows the cursor can reach in the list on screen.
    pub rows: usize,
    /// Height and length of whatever scrolls on this screen.
    pub page: (usize, usize),
}

/// Where `h` or Esc returns to.
#[derive(Debug, Clone)]
pub(crate) struct Back {
    screen: Screen,
    row: usize,
    scroll: usize,
    filter: String,
}

impl Fetch {
    /// Everything the screen shows of the fetch. The loop redraws when this changes.
    pub fn face(&self) -> String {
        let age = ui::age(self.age_ms);
        format!(
            "{} {} {} {age} {:?} {} {:?} {}",
            self.loaded,
            self.in_flight,
            self.tick,
            self.error,
            self.retry_in_s,
            self.live,
            self.why
        )
    }
}

#[derive(Debug, Clone)]
pub struct App {
    pub data: model::Data,
    pub theme: theme::Theme,
    pub screen: Screen,
    pub fetch: Fetch,
    /// The glance cursor: asks, then "to check" rows, then campaigns, then parked ones.
    pub row: usize,
    /// Roots whose rows changed since the owner last looked (the dim `•`).
    pub changed: Vec<i64>,
    /// The focused pane of the campaign view: lanes, asks, docs, PRs, log.
    pub pane: usize,
    /// The lane cursor, in tree order; 0 is the lead.
    pub lane: usize,
    /// The ask being answered. A copy, so a refresh under the dialog cannot change which
    /// ask the answer goes to.
    pub ask: Option<model::Need>,
    /// The chosen option in the answer dialog.
    pub choice: usize,
    /// The options picked in a multi-select ask, in option order.
    pub picked: Vec<usize>,
    /// A structured ask's note, sent after the picked options as ` — note`.
    pub note: String,
    /// The answer as it will be sent.
    pub text: String,
    /// Typing goes to the answer field.
    pub editing: bool,
    /// Asks answered from this view; they stay listed until the next snapshot.
    pub(crate) sent: Vec<i64>,
    /// Rows are narrowed to names containing this.
    pub filter: String,
    /// Typing goes to the filter.
    pub typing: bool,
    /// One line in place of the footer hints: what the last action did.
    pub status: Option<String>,
    /// A write waiting for `y` (park, unpark).
    pub(crate) pending: Option<actions::Job>,
    pub mouse: bool,
    /// Dark, light and terminal as this terminal can show them; `t` cycles.
    pub themes: [theme::Theme; 3],
    pub(crate) back: Vec<Back>,
    pub(crate) seen: Rc<RefCell<Seen>>,
    /// The last click, to tell a double-click.
    pub(crate) click: Option<(Instant, Hit)>,
    /// The need and check counts before the filter: the pill never follows a filter.
    pub(crate) counts: Option<(usize, usize)>,
    /// First visible line of the pager, the help, the all-campaigns list and the glance's
    /// detail pane.
    pub scroll: usize,
    /// The glance's detail pane has the keys; `row` stays on the row it shows.
    pub detail: bool,
    /// `--ascii`: draw every glyph as its ASCII stand-in.
    pub ascii: bool,
    /// The slotr view's own read, apart from the glance's: `loaded` once slotr answered,
    /// `error` while it cannot be read. The last good pools stay in `data.slotr`.
    pub slotr: Fetch,
}

impl App {
    pub fn new(data: model::Data) -> Self {
        Self {
            data,
            theme: theme::DARK,
            screen: Screen::default(),
            fetch: Fetch::default(),
            row: 0,
            changed: vec![],
            pane: 0,
            lane: 0,
            ask: None,
            choice: 0,
            picked: vec![],
            note: String::new(),
            text: String::new(),
            editing: false,
            sent: vec![],
            filter: String::new(),
            typing: false,
            status: None,
            pending: None,
            mouse: true,
            themes: [theme::DARK, theme::LIGHT, theme::TERMINAL],
            back: vec![],
            seen: Rc::default(),
            click: None,
            counts: None,
            scroll: 0,
            detail: false,
            ascii: false,
            slotr: Fetch::default(),
        }
    }

    /// The app with the filter applied to the list on screen. The clone shares `seen`.
    // ponytail: one clone of the snapshot per frame and key; filter by index if it shows.
    pub(crate) fn view(&self) -> App {
        let mut v = self.clone();
        let q = self.filter.to_lowercase();
        if q.is_empty() {
            return v;
        }
        let has = |name: &str| name.to_lowercase().contains(&q);
        match self.screen {
            Screen::Pager => {}
            Screen::AllCampaigns => v.data.roots.retain(|r| has(&r.name)),
            Screen::Campaign => {
                // Matches lose their place in the tree: a flat list under the lead.
                let root = v.data.campaign.root.id;
                v.data.campaign.lanes.retain(|l| has(&l.name));
                v.data
                    .campaign
                    .lanes
                    .iter_mut()
                    .for_each(|l| l.parent_id = root);
            }
            _ => {
                let g = &mut v.data.glance;
                v.counts = Some((g.needs_you.len(), g.attention.len()));
                g.needs_you.retain(|a| has(&a.campaign));
                g.attention.retain(|a| has(&a.campaign));
                g.campaigns.retain(|c| has(&c.name));
            }
        }
        v
    }

    /// Takes a new snapshot and keeps the cursor on the row it was on.
    pub fn snapshot(&mut self, glance: model::Glance) {
        let at = |app: &App| glance::ids(&app.view().data.glance);
        // ponytail: under the all-campaigns list or the slotr view `row` is that
        // screen's cursor, so the glance row saved for the way back is not followed.
        // The slotr view's cursor is `row` too.
        let own = |s: Screen| matches!(s, Screen::AllCampaigns | Screen::Slotr);
        let glance_row = !own(self.screen) && self.back.iter().all(|b| !own(b.screen));
        let was = at(self).get(self.row).cloned();
        let before = was.clone().filter(|_| glance_row);
        self.sent
            .retain(|id| glance.needs_you.iter().any(|a| a.ask_id == *id));
        // "Changed since you looked": a campaign whose last event, owner note, lead, lanes
        // or parking differ from the snapshot on screen. Any key clears the marks.
        let event = |last: &Option<model::Last>| last.as_ref().map(|l| l.event_id);
        let face = |c: &model::CampaignRow| {
            let lanes = (c.lanes.working, c.lanes.ready, c.lanes.open);
            (
                event(&c.last),
                event(&c.owner_note),
                c.lead.clone(),
                lanes,
                c.parked,
            )
        };
        for new in glance.campaigns.iter().filter(|_| self.fetch.loaded) {
            let old = self.data.glance.campaigns.iter().find(|c| c.id == new.id);
            if old.map(face) != Some(face(new)) && !self.changed.contains(&new.id) {
                self.changed.push(new.id);
            }
        }
        // ponytail: the all-campaigns list is the open roots the glance names. Closed
        // ones need a list read that taskr does not have yet.
        let quiet = glance.quiet.names.iter().zip(&glance.quiet.root_ids);
        self.data.roots = glance
            .campaigns
            .iter()
            .map(|c| model::RootRow {
                id: c.id,
                name: c.name.clone(),
                status: "open".into(),
                parked: c.parked,
                host: c.host.clone(),
                lanes_open: c.lanes.open,
                lanes_total: c.lanes.open,
                activity_age_ms: c.activity_age_ms,
            })
            .chain(quiet.map(|(name, id)| model::RootRow {
                id: *id,
                name: name.clone(),
                status: "open".into(),
                ..model::RootRow::default()
            }))
            .collect();
        self.data.glance = glance;
        if let Some(i) = before.and_then(|id| at(self).iter().position(|r| *r == id)) {
            self.row = i;
        }
        // The detail pane shows another row now: it starts at its top.
        if glance_row && at(self).get(self.row) != was.as_ref() {
            match self.back.first_mut() {
                Some(b) if b.screen == Screen::Glance => b.scroll = 0,
                _ => self.scroll = 0,
            }
        }
    }

    pub(crate) fn hit(&self, area: Rect, hit: Hit) {
        self.seen.borrow_mut().hits.push((area, hit));
    }

    /// Opens the answer dialog on `ask`, with its first option chosen.
    pub fn answer(&mut self, ask: model::Need) {
        self.go(Screen::Answer);
        self.ask = Some(ask);
        (self.picked, self.note) = (vec![], String::new());
        self.choose(0);
    }

    pub(crate) fn choose(&mut self, choice: usize) {
        let Some(ask) = &self.ask else { return };
        let p = ask::parse(ask);
        self.choice = choice.min(p.options.len());
        self.editing = self.choice == p.options.len();
        // Choosing an option fills the field with what it meant, so the ledger records it.
        // A multi-select ask's field is the picked set, wherever the cursor is.
        self.text = if self.editing {
            String::new()
        } else if p.multi {
            ask::answer(&p.options, &self.picked, &self.note)
        } else {
            ask::answer(&p.options, &[self.choice], &self.note)
        };
    }

    /// Picks or drops the option under the cursor of a multi-select ask.
    pub(crate) fn toggle(&mut self) {
        match self.picked.binary_search(&self.choice) {
            Ok(i) => {
                self.picked.remove(i);
            }
            Err(i) => self.picked.insert(i, self.choice),
        }
        self.choose(self.choice);
    }

    /// Changes what the owner types: a structured ask's note while an option is chosen,
    /// the answer itself otherwise.
    pub(crate) fn edit(&mut self, change: impl FnOnce(&mut String)) {
        let structured = self.ask.as_ref().is_some_and(|a| a.question.is_some());
        let on_option = self
            .ask
            .as_ref()
            .is_some_and(|a| self.choice < ask::parse(a).options.len());
        if structured && on_option {
            change(&mut self.note);
            let editing = self.editing;
            self.choose(self.choice);
            self.editing = editing;
        } else {
            change(&mut self.text);
        }
    }

    /// Opens `screen` over the current one.
    pub(crate) fn go(&mut self, screen: Screen) {
        self.back.push(Back {
            screen: self.screen,
            row: self.row,
            scroll: self.scroll,
            filter: std::mem::take(&mut self.filter),
        });
        (self.screen, self.scroll, self.typing) = (screen, 0, false);
    }

    /// Closes the current screen; false on the glance, where there is nothing to close.
    pub(crate) fn close(&mut self) -> bool {
        let Some(back) = self.back.pop() else {
            return false;
        };
        (self.screen, self.row, self.scroll, self.filter) =
            (back.screen, back.row, back.scroll, back.filter);
        (self.typing, self.editing) = (false, false);
        true
    }

    /// The screen under an overlay: what the help describes.
    pub(crate) fn under(&self) -> Screen {
        self.back.last().map_or(Screen::Glance, |b| b.screen)
    }
}

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    *app.seen.borrow_mut() = Seen::default();
    let app = &app.view();
    // Under 8 rows the section rules leave no room for the rows they head.
    if area.width < 30 || area.height < 8 {
        ui::pill_only(f, app);
    } else {
        screen(f, app);
        notice(f, app);
    }
    ui::plain_emoji(f.buffer_mut());
    if app.ascii {
        ui::ascii(f.buffer_mut());
    }
}

/// Draws a frame. A screen other than the last frame's is written whole: the diff only
/// writes what changed, so anything the terminal shows differently from the buffer would
/// otherwise outlive the screen it came from. Every cell is sent again with no erase first,
/// so nothing flashes blank. `shown` is the last frame's screen; `None` (Ctrl-L) clears the
/// terminal and paints the next frame whole whatever it shows.
pub fn frame<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &App,
    shown: &mut Option<Screen>,
) -> Result<(), B::Error> {
    if shown.is_none() {
        terminal.clear()?;
    }
    let whole = *shown != Some(app.screen);
    terminal.draw(|f| {
        draw(f, app);
        if whole {
            for cell in &mut f.buffer_mut().content {
                cell.diff_option = CellDiffOption::AlwaysUpdate;
            }
        }
    })?;
    *shown = Some(app.screen);
    Ok(())
}

fn screen(f: &mut Frame, app: &App) {
    match app.screen {
        Screen::Glance => glance::draw(f, app),
        Screen::Campaign => campaign::draw(f, app),
        Screen::Answer | Screen::Confirm => dialog::answer(f, app),
        Screen::Help => dialog::help(f, app),
        Screen::AllCampaigns => lists::all_campaigns(f, app),
        Screen::Pager => lists::pager(f, app),
        Screen::Slotr => slotr::draw(f, app),
    }
}

/// In place of the footer hints: a question waiting for `y`, or what the last action did.
fn notice(f: &mut Frame, app: &App) {
    let t = &app.theme;
    let line = match (&app.pending, &app.status) {
        (Some(job), _) => vec![
            ui::bold(format!(" {}? ", job.label()), t.check),
            ui::bold("y", t.accent),
            ui::sp(" yes · any other key cancels", t.dim),
        ],
        (None, Some(status)) => {
            let color = match status.chars().next() {
                Some('✓') => t.ok,
                Some('!') => t.check,
                _ => t.text,
            };
            vec![ui::sp(format!(" {status}"), color)]
        }
        _ => return,
    };
    let area = f.area();
    let foot = Rect {
        y: area.bottom() - 1,
        height: 1,
        ..area
    };
    app.seen.borrow_mut().hits.retain(|(r, _)| r.y != foot.y);
    f.render_widget(ratatui::widgets::Clear, foot);
    f.render_widget(
        Paragraph::new(Line::from(ui::fit(line, foot.width as usize))),
        foot,
    );
}
