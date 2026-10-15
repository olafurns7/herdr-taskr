//! taskr-tui: the owner's terminal view of the taskr ledger (plan §2 and §3).
//!
//! Every screen draws from a [`model::Data`] value; [`frames`] renders them from a fixture
//! as golden frames. [`client`] reads the ledger in the background, [`input`] turns keys
//! and the mouse into state changes and [`actions`] jobs, and `main` runs the loop.

pub mod actions;
pub mod client;
pub mod config;
pub mod frames;
pub mod github;
pub mod health;
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
    /// The Dashboard: the panels, as toggled.
    #[default]
    Glance,
    /// Every open owner ask, whatever the Dashboard's panels show.
    Asks,
    /// Every listed campaign, whatever the Dashboard's panels show.
    Fleet,
    /// One row's item in full ([`App::target`]).
    Detail,
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

impl Screen {
    /// The top-level screens, in tab order: `D A F I`, `[ ]` cycle them.
    pub(crate) const PEERS: [Screen; 4] =
        [Screen::Glance, Screen::Asks, Screen::Fleet, Screen::Detail];

    pub(crate) fn peer(self) -> Option<usize> {
        Self::PEERS.iter().position(|s| *s == self)
    }

    /// A top-level screen of rows: `row` is its cursor.
    pub(crate) fn rows(self) -> bool {
        matches!(self, Screen::Glance | Screen::Asks | Screen::Fleet)
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Screen::Asks => "Asks",
            Screen::Fleet => "Fleet",
            Screen::Detail => "Detail",
            _ => "Dashboard",
        }
    }
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

/// A top-level screen's place while another one is shown: its row by identity, so a
/// snapshot in between cannot move the cursor to another item.
#[derive(Debug, Clone, Default)]
pub(crate) struct Place {
    id: Option<glance::RowId>,
    row: usize,
    scroll: usize,
    filter: String,
}

/// What the Detail screen shows: the item `I` was pressed on, by identity, and the screen
/// and filter it was listed under. It is never another row: when the item is gone, Detail
/// says so.
#[derive(Debug, Clone)]
pub(crate) struct Target {
    pub(crate) id: glance::RowId,
    pub(crate) from: Screen,
    pub(crate) filter: String,
}

/// What the Decisions and Decision log panels (7, 9) read: `data.campaign`, but only once
/// a campaign the owner opened on purpose (`l`, or `o` on their rows) has arrived. Glance `r`
/// does not refresh it; opening the campaign again does.
#[derive(Debug, Clone, Default)]
pub struct Scope {
    /// When the campaign on hand was read, on the observation clock. None: nothing opened.
    pub read_at: Option<i64>,
    /// The last failed read of the opened campaign, or of one being opened. The last good
    /// read stays on hand under it.
    pub error: Option<String>,
    /// Bumped by every deliberate open. A reply from before the latest one is dropped.
    pub(crate) generation: u64,
    /// The deliberate open still awaited, and the screen and depth it was asked from.
    pub(crate) want: Option<(i64, Screen, usize)>,
}

impl Scope {
    /// Age of the read on hand, as the panels show it.
    pub(crate) fn age(&self, at: i64) -> Option<String> {
        self.read_at
            .map(|read| ui::age(at.saturating_sub(read).saturating_mul(1000)))
    }
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
    /// First visible line of the pager, the help, the all-campaigns list and Detail.
    pub scroll: usize,
    /// Each top-level screen's place while it is not shown, in [`Screen::PEERS`] order.
    pub(crate) places: [Place; 4],
    /// The Detail screen's item.
    pub(crate) target: Option<Target>,
    /// Open owner asks and listed campaigns before panels and filter: the tab line's
    /// counts. Filled by [`App::view`].
    pub(crate) listed: (usize, usize),
    /// `--ascii`: draw every glyph as its ASCII stand-in.
    pub ascii: bool,
    /// The slotr view's own read, apart from the glance's: `loaded` once slotr answered,
    /// `error` while it cannot be read. The last good pools stay in `data.slotr`.
    pub slotr: Fetch,
    pub config: config::Config,
    pub config_warning: Option<String>,
    pub github: github::Dashboard,
    pub shipped: github::Dashboard,
    pub builds: github::Dashboard,
    /// Issues by assignee (panel 0): the fixed repository only, never `config.repos`.
    pub issues: github::Dashboard,
    /// Health (panel H): the hub's latest HEALTH note, read by its own poller.
    pub health: health::Health,
    /// One clock observation shared by the rendered view and its input identities.
    pub observed_at: i64,
    pub observed_tick: Instant,
    pub scope: Scope,
    /// Fleet's rows (panel 8): the snapshot's listed campaigns, whether or not panel 3
    /// shows them. Filled by [`App::view`]; empty on the app itself.
    pub(crate) fleet: Vec<model::CampaignRow>,
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
            places: Default::default(),
            target: None,
            listed: (0, 0),
            ascii: false,
            slotr: Fetch::default(),
            config: config::Config::default(),
            config_warning: None,
            github: github::Dashboard::default(),
            shipped: github::Dashboard::default(),
            builds: github::Dashboard::default(),
            issues: github::Dashboard::default(),
            health: health::Health::default(),
            observed_at: time::OffsetDateTime::now_utc().unix_timestamp(),
            observed_tick: Instant::now(),
            scope: Scope::default(),
            fleet: vec![],
        }
    }

    pub fn advance_time(&mut self, at: i64, tick: Instant) -> bool {
        // Issues groups and their counts too: a surviving group can lose a closure.
        let signature = |app: &App, at| {
            let shipped = app
                .shipped
                .authors(at)
                .into_iter()
                .map(|(author, rows)| {
                    (
                        author,
                        rows.iter()
                            .map(|p| (p.repo.clone(), p.number))
                            .collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>();
            (shipped, app.issues.assignees(at))
        };
        // A cached panel's age is on screen: a new minute is a new frame.
        let aged = (self.config.panels[6] || self.config.panels[8])
            && self.scope.age(self.observed_at) != self.scope.age(at);
        if signature(self, self.observed_at) == signature(self, at) {
            self.observed_at = at;
            self.observed_tick = tick;
            return aged;
        }
        let view = self.glance_view();
        let selected = glance::ids(&view).get(view.row).cloned();
        self.observed_at = at;
        self.observed_tick = tick;
        self.restore_row(selected);
        self.click = None;
        *self.seen.borrow_mut() = Seen::default();
        true
    }

    /// The app with the filter applied to the list on screen. The clone shares `seen`.
    // ponytail: one clone of the snapshot per frame and key; filter by index if it shows.
    pub(crate) fn view(&self) -> App {
        let mut v = self.clone();
        // Detail lists as the screen its item came from did, under that screen's filter.
        let list = match (&self.target, self.screen) {
            (Some(t), Screen::Detail) => {
                v.filter = t.filter.clone();
                t.from
            }
            _ => self.screen,
        };
        // Asks and Fleet list their rows whatever the Dashboard's panels show.
        let only = |i: usize| -> [bool; 11] { std::array::from_fn(|p| p == i) };
        match list {
            Screen::Asks => v.config.panels = only(0),
            Screen::Fleet => v.config.panels = only(2),
            _ => {}
        }
        let panels = v.config.panels;
        let q = v.filter.to_lowercase();
        let g = &self.data.glance;
        v.listed = (g.needs_you.len(), g.campaigns.len());
        // Fleet reads the snapshot itself, so hiding panel 3 does not empty it. Every
        // screen fills it: the help draws the glance behind itself.
        v.fleet = if panels[7] && self.fetch.loaded {
            let all = &self.data.glance.campaigns;
            all.iter()
                .filter(|c| !c.parked)
                .chain(all.iter().filter(|c| c.parked))
                .cloned()
                .collect()
        } else {
            vec![]
        };
        if list.rows() {
            let g = &mut v.data.glance;
            v.counts = Some((g.needs_you.len(), g.attention.len()));
            if !panels[0] {
                g.needs_you.clear();
            }
            if !panels[1] {
                g.attention.clear();
            }
            if !panels[2] {
                g.campaigns.clear();
                g.quiet.count = 0;
            }
            if !panels[3] {
                v.github.sources.clear();
            }
            if !panels[4] {
                v.shipped.sources.clear();
            }
            if !panels[5] {
                v.builds.sources.clear();
            }
            if !panels[9] {
                v.issues.sources.clear();
            }
            for source in &mut v.github.sources {
                source.prs.retain(|p| {
                    format!("{} {} {} #{}", p.repo, p.title, p.author, p.number)
                        .to_lowercase()
                        .contains(&q)
                });
            }
        }
        if !q.is_empty() {
            v.filtered(list, &q);
        }
        if self.screen == Screen::Detail {
            let ids = glance::ids(&v);
            v.row = self
                .target
                .as_ref()
                .and_then(|t| ids.iter().position(|r| *r == t.id))
                .unwrap_or(ids.len());
            // An ask's or a check's campaign activity shows with campaigns hidden too: with
            // panel 3 off no campaign row is listed, so this moves no identity.
            if !panels[2] {
                v.data.glance.campaigns = self.data.glance.campaigns.clone();
            }
        }
        v
    }

    /// Narrows the rows of `list` to names containing `q`.
    fn filtered(&mut self, list: Screen, q: &str) {
        let v = self;
        let has = |name: &str| name.to_lowercase().contains(q);
        match list {
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
                v.counts = Some(
                    v.counts
                        .unwrap_or((v.data.glance.needs_you.len(), v.data.glance.attention.len())),
                );
                let g = &mut v.data.glance;
                g.needs_you.retain(|a| has(&a.campaign));
                g.attention.retain(|a| has(&a.campaign));
                g.campaigns.retain(|c| has(&c.name));
                // Decisions and the decision log are one campaign's; the filter leaves them.
                v.fleet.retain(|c| has(&c.name));
            }
        }
    }

    /// Takes a new snapshot and keeps the cursor on the row it was on.
    pub fn snapshot(&mut self, glance: model::Glance) {
        let at = |app: &App| glance::ids(&app.view());
        // ponytail: under the all-campaigns list or the slotr view `row` is that
        // screen's cursor, so the glance row saved for the way back is not followed.
        // The slotr view's cursor is `row` too.
        // Detail has no cursor: its item is found by identity.
        let own = |s: Screen| matches!(s, Screen::AllCampaigns | Screen::Slotr | Screen::Detail);
        let glance_row = !own(self.screen) && self.back.iter().all(|b| !own(b.screen));
        let ids = at(self);
        let before = ids.get(self.row).cloned().filter(|_| glance_row);
        let saved = {
            let view = self.glance_view();
            glance::ids(&view).get(view.row).cloned()
        };
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
        if glance_row {
            let len = at(self).len();
            self.row = self.row.min(len.saturating_sub(1));
        }
        // Under an overlay the screen's row waits in `back`, and Esc returns to it.
        if glance_row && !self.screen.rows() && self.back.first().is_some_and(|b| b.screen.rows()) {
            self.restore_row(saved);
        }
        // Rows that moved are not where the last click landed.
        if at(self) != ids {
            self.click = None;
        }
    }

    /// The top-level screen under any overlay, with its own row and filter.
    pub(crate) fn glance_view(&self) -> App {
        let mut app = self.clone();
        if app.screen.peer().is_none() {
            match app.back.first() {
                Some(back) => {
                    app.screen = back.screen;
                    app.row = back.row;
                    app.filter = back.filter.clone();
                }
                None => app.screen = Screen::Glance,
            }
        }
        app.view()
    }

    pub(crate) fn restore_row(&mut self, before: Option<glance::RowId>) {
        let view = self.glance_view();
        let ids = glance::ids(&view);
        let row = before
            .as_ref()
            .and_then(|id| ids.iter().position(|r| r == id))
            .unwrap_or(view.row.min(ids.len().saturating_sub(1)));
        let changed = row != view.row || ids.get(row) != before.as_ref();
        if self.screen.rows() {
            self.row = row;
            if changed {
                self.scroll = 0;
            }
        } else if self.screen.peer().is_none()
            && let Some(back) = self.back.first_mut()
            && back.screen.rows()
        {
            back.row = row;
            if changed {
                back.scroll = 0;
            }
        }
        if changed {
            self.click = None;
        }
    }

    /// Shows the top-level screen `to`, closing any overlay first. The screen left keeps
    /// its row by identity, its scroll and its filter; `to` gets its own back. Nothing is
    /// read here: sources keep their own schedules.
    pub(crate) fn switch(&mut self, to: Screen) {
        while self.close() {}
        let (Some(from), Some(i)) = (self.screen.peer(), to.peer()) else {
            return;
        };
        if from == i {
            return;
        }
        let id = self
            .screen
            .rows()
            .then(|| glance::ids(&self.view()).get(self.row).cloned())
            .flatten();
        self.places[from] = Place {
            id,
            row: self.row,
            scroll: self.scroll,
            filter: std::mem::take(&mut self.filter),
        };
        let place = std::mem::take(&mut self.places[i]);
        (self.screen, self.row, self.scroll, self.filter) =
            (to, place.row, place.scroll, place.filter);
        (self.typing, self.click) = (false, None);
        if to.rows() {
            let scroll = self.scroll;
            self.restore_row(place.id);
            self.scroll = scroll;
        }
    }

    /// `I`: the selected row's item in full on the Detail screen. With no row selected,
    /// Detail shows the item it showed last, or says to select one.
    pub(crate) fn inspect(&mut self) {
        while self.close() {}
        if self.screen.rows() {
            let view = self.view();
            if let Some(id) = glance::ids(&view).get(self.row).cloned()
                && !matches!(glance::selected(&view), glance::Selected::Nothing)
            {
                let target = Target {
                    id,
                    from: self.screen,
                    filter: self.filter.clone(),
                };
                self.switch(Screen::Detail);
                (self.target, self.scroll) = (Some(target), 0);
                return;
            }
        }
        self.switch(Screen::Detail);
    }

    /// Whether panel `i`'s source may start a read: its card is shown on the Dashboard, and
    /// the Dashboard is on top, under no overlay or dialog.
    pub fn admits(&self, i: usize) -> bool {
        // A pending `y` question is a modal too: it owns every key until answered.
        self.screen == Screen::Glance && self.pending.is_none() && self.config.panels[i]
    }

    /// Esc or `h` on Detail: back to the screen, row, scroll and filter it came from.
    pub(crate) fn uninspect(&mut self) {
        self.switch(self.target.as_ref().map_or(Screen::Glance, |t| t.from));
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
        if app.screen == Screen::Glance && !app.config.panels.iter().any(|p| *p) && area.height > 1
        {
            f.render_widget(
                Paragraph::new("1-9, 0, H show hidden panels"),
                Rect {
                    y: area.y + 1,
                    height: 1,
                    ..area
                },
            );
            if area.height > 2
                && let Some(warning) = &app.config_warning
            {
                f.render_widget(
                    Paragraph::new(warning.as_str()),
                    Rect {
                        y: area.y + 2,
                        height: 1,
                        ..area
                    },
                );
            }
        }
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
        Screen::Glance | Screen::Asks | Screen::Fleet => glance::draw(f, app),
        Screen::Detail => glance::draw_detail(f, app),
        Screen::Campaign => campaign::draw(f, app),
        Screen::Answer | Screen::Confirm => dialog::answer(f, app),
        Screen::Help => dialog::help(f, app),
        Screen::AllCampaigns => lists::all_campaigns(f, app),
        Screen::Pager => lists::pager(f, app),
        Screen::Slotr => slotr::draw(f, app),
    }
}

/// The top-level screen under an overlay, as it is shown there: what a dialog draws behind
/// itself.
pub(crate) fn background(f: &mut Frame, app: &App) {
    let mut under = app.glance_view();
    under.scroll = app.back.first().map_or(app.scroll, |b| b.scroll);
    screen(f, &under);
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
