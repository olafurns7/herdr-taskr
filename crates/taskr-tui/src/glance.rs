//! Level 1: the Dashboard (needs you, to check, campaigns and the toggled panels), the Asks
//! and Fleet screens (the same rows, asks or campaigns only) in one full-width column, and
//! the Detail screen: one row's item in full.

use std::ops::Range;

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::{Line, Span},
    widgets::Paragraph,
};

use ratatui::style::Color;

use crate::{
    App, Hit, Screen, ask,
    github::Kind,
    keys::hint,
    model::{Attention, Campaign, CampaignRow, Event, Glance, Need},
    theme::Theme,
    ui::{self, age, bold, clip, one_line, pad, sp, wrap},
};
use unicode_width::UnicodeWidthStr;

/// The panels between the checks and the campaigns, in priority order: GitHub's two,
/// then the ledger's Decisions (7), Fleet (8) and Decision log (9), then Issues (0), then
/// Health (H).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Panel {
    Gh(Kind),
    Decisions,
    Fleet,
    Log,
    Health,
}

const PANELS: [Panel; 7] = [
    Panel::Gh(Kind::Shipped),
    Panel::Gh(Kind::Build),
    Panel::Decisions,
    Panel::Fleet,
    Panel::Log,
    Panel::Gh(Kind::Issues),
    Panel::Health,
];

fn github_shown(app: &App, kind: Kind) -> bool {
    let (index, unset) = match kind {
        Kind::Shipped => (4, app.config.repos.is_empty()),
        Kind::Build => (5, app.config.repos.is_empty()),
        Kind::Issues => (9, app.config.issues_repo.is_none()),
    };
    app.config.panels[index] && (unset || !kind.dashboard(app).sources.is_empty())
}

/// The panels on screen. A hidden GitHub panel's sources are cleared by `App::view`.
fn shown(app: &App) -> Vec<Panel> {
    PANELS
        .into_iter()
        .filter(|p| match p {
            Panel::Gh(kind) => github_shown(app, *kind),
            Panel::Decisions => app.config.panels[6],
            Panel::Fleet => app.config.panels[7],
            Panel::Log => app.config.panels[8],
            Panel::Health => app.config.panels[10],
        })
        .collect()
}

/// The panels drawn from a campaign read or the glance, apart from the external sources.
fn ledger_panel(panel: &Panel) -> bool {
    matches!(panel, Panel::Decisions | Panel::Fleet | Panel::Log)
}

/// The campaign panels 7 and 9 read: the last one opened on purpose, once it arrived.
/// Never the cursor's campaign, and never a total over campaigns.
pub(crate) fn scoped(app: &App) -> Option<&Campaign> {
    app.scope.read_at.map(|_| &app.data.campaign)
}

/// Decisions in force, from the campaign read on hand.
fn decisions(app: &App) -> &[Event] {
    match scoped(app) {
        Some(c) if app.config.panels[6] => &c.decisions,
        _ => &[],
    }
}

/// The decision events in the read's recent log sample: history, apart from those in force.
fn logged(app: &App) -> Vec<&Event> {
    scoped(app)
        .filter(|_| app.config.panels[8])
        .map_or(vec![], |c| {
            c.log.iter().filter(|e| e.kind == "decision").collect()
        })
}

/// One row of a card: its lines, drawn whole or not at all, and a second line a campaign
/// gets when the card has room, the most recently active first.
struct Row {
    lines: Vec<Line<'static>>,
    extra: Option<(i64, Line<'static>)>,
    /// Only the first line is the cursor's (Health's wrapped values).
    once: bool,
}

/// A Dashboard card before it is fitted to its height: the rule, the lines under it, its
/// rows and how its overflow line names what is cut. A `true` head is an honest notice
/// (stale, unavailable, capped, empty, the read's age) and stays at every height.
struct Card {
    title: &'static str,
    right: String,
    color: Color,
    heads: Vec<(Line<'static>, bool)>,
    rows: Vec<Row>,
    /// Lines after the rows, given only spare room (Health's source).
    foot: Vec<Line<'static>>,
    /// Drawn under the rows while none is cut (the quiet count).
    tail: Option<Line<'static>>,
    /// The cursor index of the first row.
    offset: usize,
    /// Rows the floor keeps: two asks ("asks never fold"), one of anything else.
    keep: usize,
    /// Singular and plural, as the overflow line counts rows.
    unit: (&'static str, &'static str),
    /// Where every row is listed, for the overflow line; empty: tab to the card.
    route: &'static str,
    /// After the route: what the snapshot folded without listing.
    note: String,
    /// The rule shows the window's position, `1/3`.
    counted: bool,
    /// Drawn from a stale ledger snapshot: dimmed.
    dim: bool,
}

/// The text of a line, for the overflow cues.
fn plain(line: &Line) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

impl Card {
    fn new(app: &App, title: &'static str, right: String, color: Color, offset: usize) -> Self {
        Card {
            title,
            right,
            color,
            heads: vec![],
            rows: vec![],
            foot: vec![],
            tail: None,
            offset,
            keep: 1,
            unit: ("row", "rows"),
            route: "",
            note: String::new(),
            counted: false,
            dim: false,
        }
        .dim_if(false, app)
    }

    fn dim_if(mut self, ledger: bool, app: &App) -> Self {
        self.dim = ledger && app.fetch.loaded && app.fetch.error.is_some();
        self
    }

    /// A notice line, in the rows' one-cell gutter, so a right column never joins the left.
    fn head(&mut self, text: impl Into<String>, color: Color, keep: bool, width: usize) {
        let text = clip(&text.into(), width.saturating_sub(1));
        self.heads
            .push((Line::from(vec![Span::raw(" "), sp(text, color)]), keep));
    }

    fn cursor(&self, row: usize) -> Option<usize> {
        row.checked_sub(self.offset)
            .filter(|i| *i < self.rows.len())
    }

    fn lines(&self, rows: &Range<usize>) -> usize {
        self.rows[rows.clone()].iter().map(|r| r.lines.len()).sum()
    }

    /// Every row, notice and foot line; a campaign's second lines only from spare rows.
    fn want(&self) -> usize {
        1 + self.heads.len()
            + self.lines(&(0..self.rows.len()))
            + self.foot.len()
            + usize::from(self.tail.is_some())
    }

    fn full(&self) -> usize {
        self.want() + self.rows.iter().filter(|r| r.extra.is_some()).count()
    }

    /// The rows around the cursor that fit `room` lines, the cursor's last where it can be;
    /// the cursor's row, or the first, even when it does not fit.
    fn window(&self, room: usize, cursor: Option<usize>) -> Range<usize> {
        let n = self.rows.len();
        if n == 0 {
            return 0..0;
        }
        let at = cursor.unwrap_or(0);
        let mut w = at..at + 1;
        loop {
            let back = w.start > 0 && self.lines(&(w.start - 1..w.end)) <= room;
            let on = w.end < n && self.lines(&(w.start..w.end + 1)) <= room;
            match (back, on) {
                (true, _) if cursor.is_some() => w.start -= 1,
                (_, true) => w.end += 1,
                (true, _) => w.start -= 1,
                _ => return w,
            }
        }
    }

    /// The rule, the honest notices, the cursor's window of `keep` rows and the overflow
    /// line (or the tail).
    fn floor(&self, row: usize) -> usize {
        let n = self.rows.len();
        let keep = self.keep.min(n);
        let start = self
            .cursor(row)
            .map_or(0, |i| (i + 1).saturating_sub(keep))
            .min(n - keep);
        1 + self.heads.iter().filter(|h| h.1).count()
            + self.lines(&(start..start + keep))
            + usize::from(n > keep || self.tail.is_some())
    }

    /// The card in `height` lines, each with the cursor row it belongs to.
    fn fit(&self, t: &Theme, height: usize, row: usize, width: usize) -> Drawn {
        let cursor = self.cursor(row);
        let n = self.rows.len();
        let all = height >= self.full();
        let kept = self.heads.iter().filter(|h| h.1).count();
        let room = height.saturating_sub(1 + kept);
        // Every row when they fit with the tail: the more line is spent only on a cut.
        let rows = if all || self.lines(&(0..n)) + usize::from(self.tail.is_some()) <= room {
            0..n
        } else {
            self.window(room.saturating_sub(1), cursor)
        };
        let more = n - rows.len();
        let mut spare =
            room.saturating_sub(self.lines(&rows) + usize::from(more > 0 || self.tail.is_some()));
        let mut heads = vec![];
        for (line, keep) in &self.heads {
            if *keep || all || spare > 0 {
                if !*keep && !all {
                    spare -= 1;
                }
                heads.push(line.clone());
            }
        }
        let mut seconds: Vec<usize> = rows
            .clone()
            .filter(|i| self.rows[*i].extra.is_some())
            .collect();
        seconds.sort_by_key(|i| self.rows[*i].extra.as_ref().map(|e| e.0));
        if !all {
            seconds.truncate(spare);
            spare -= seconds.len();
        }
        let right = if self.counted {
            count(&rows, n, cursor)
        } else {
            self.right.clone()
        };
        let mut out = Drawn::default();
        out.push(ui::rule(t, self.title, &right, self.color, width), None);
        // An empty card's state is its one inert stop: the cursor reaches it, nothing acts.
        let mut stop = self.rows.is_empty();
        for line in heads {
            if stop {
                stop = false;
                let on = row == self.offset;
                // ui::row brings its own gutter (or the cursor bar) in place of the head's.
                out.push(
                    ui::row(t, line.spans[1..].to_vec(), on, false, self.color, width),
                    Some(self.offset),
                );
            } else {
                out.push(line, None);
            }
        }
        for i in rows {
            for (n, line) in self.rows[i].lines.iter().enumerate() {
                let hit = (n == 0 || !self.rows[i].once).then_some(self.offset + i);
                out.push(line.clone(), hit);
            }
            if seconds.contains(&i)
                && let Some((_, line)) = &self.rows[i].extra
            {
                out.push(line.clone(), Some(self.offset + i));
            }
        }
        if more > 0 {
            let unit = if more == 1 { self.unit.0 } else { self.unit.1 };
            let route = match (cursor, self.route) {
                (Some(_), _) => "j k",
                (None, "") => "tab here",
                (None, route) => route,
            };
            out.push(
                Line::from(sp(
                    clip(
                        &format!(" +{more} more {unit} · {route}{}", self.note),
                        width,
                    ),
                    t.dim,
                )),
                None,
            );
        } else if let Some(tail) = &self.tail {
            out.push(tail.clone(), None);
        }
        for line in self.foot.iter().take(if all { usize::MAX } else { spare }) {
            out.push(line.clone(), None);
        }
        out.lines.truncate(height);
        out.hits.truncate(height);
        if self.dim {
            out.dim(t);
        }
        out
    }

    /// Tiny: one line, the cursor's row (or the first) with its position; every row on its
    /// own line when `each` (the asks, a screen of one card).
    fn tiny(&self, app: &App, width: usize, each: bool) -> Drawn {
        let mut out = Drawn::default();
        let n = self.rows.len();
        let line = |i: usize, prefix: String| {
            let (text, color) = label(app, self.offset + i).unwrap_or_default();
            ui::row(
                &app.theme,
                vec![sp(
                    clip(&format!("{prefix}{text}"), width.saturating_sub(1)),
                    color,
                )],
                app.row == self.offset + i,
                false,
                color,
                width,
            )
        };
        if n == 0 {
            let state = self.heads.iter().find(|h| h.1).map(|h| plain(&h.0));
            let text = format!("{} · {}", self.title, state.unwrap_or_default().trim());
            let on = app.row == self.offset;
            let t = &app.theme;
            let line = ui::row(
                t,
                vec![sp(clip(&text, width.saturating_sub(1)), t.sub)],
                on,
                false,
                t.sub,
                width,
            );
            out.push(line, Some(self.offset));
        } else if each {
            for i in 0..n {
                out.push(line(i, String::new()), Some(self.offset + i));
            }
        } else {
            let i = self.cursor(app.row).unwrap_or(0);
            let at = if n > 1 {
                format!("{}/{n} ", i + 1)
            } else {
                String::new()
            };
            out.push(line(i, at), Some(self.offset + i));
        }
        if self.dim {
            out.dim(&app.theme);
        }
        out
    }

    /// What the overflow cues say of a card out of view: its rows, or its state.
    fn brief(&self) -> String {
        if self.rows.is_empty() {
            let state = self.heads.iter().find(|h| h.1).map(|h| plain(&h.0));
            format!("{} {}", self.title, state.unwrap_or_default().trim())
        } else {
            format!("{} {}", self.title, self.rows.len())
        }
    }
}

/// Lines and, for each, the cursor row it draws.
#[derive(Default)]
struct Drawn {
    lines: Vec<Line<'static>>,
    hits: Vec<Option<usize>>,
}

impl Drawn {
    fn push(&mut self, line: Line<'static>, hit: Option<usize>) {
        self.lines.push(line);
        self.hits.push(hit);
    }

    fn dim(&mut self, t: &Theme) {
        for line in &mut self.lines {
            for span in &mut line.spans {
                span.style.fg = Some(t.dim);
            }
        }
    }
}

/// `2`, or `3/9` when the section is longer than its window.
fn count(rows: &Range<usize>, total: usize, cursor: Option<usize>) -> String {
    if rows.len() == total {
        return total.to_string();
    }
    format!("{}/{total}", cursor.map_or(rows.end, |i| i + 1))
}

/// What Enter does on the row, at its right: `⏎` focuses the pane, `host ↗` is another
/// machine. Nothing when the row has no pane, or the snapshot does not say where it is.
fn place(t: &Theme, g: &Glance, host: &str, pane: &str) -> Span<'static> {
    if pane.is_empty() || (g.server_host.is_empty() && host.is_empty()) {
        Span::raw("")
    } else if g.local(host) {
        sp("⏎ ", t.accent)
    } else {
        sp(format!("{} ↗ ", ui::host_name(g, host)), t.remote)
    }
}

/// A "to check" row's name: its campaign, or what the check is about when it has none.
pub(crate) fn check_name(g: &Glance, a: &Attention) -> String {
    if !a.campaign.is_empty() {
        a.campaign.clone()
    } else if a.kind.starts_with("daemon") {
        "daemon".to_string()
    } else if a.kind.starts_with("host") {
        format!("host {}", ui::host_name(g, &a.host))
    } else {
        a.kind.replace('_', " ")
    }
}

/// A two-line row's second line.
fn second(text: &str, color: Color, width: usize) -> Vec<Span<'static>> {
    vec![
        Span::raw("  "),
        sp(clip(&one_line(text), width.saturating_sub(4)), color),
    ]
}

/// An ask's two lines, as the Needs you card and the picker draw it.
pub(crate) fn ask_lines(app: &App, a: &Need, on: bool, width: usize) -> Vec<Line<'static>> {
    let (t, g) = (&app.theme, &app.data.glance);
    let body = width.saturating_sub(1);
    // ◆ a structured ask, ◇ options found in its text; HUB: the hub's own dialog.
    let mut left = vec![
        bold("? ", t.need),
        bold(format!("{} ", ask::mark(a)), t.accent),
        bold(pad(&a.campaign, 20.min(body.saturating_sub(24))), t.text),
    ];
    if a.blocking {
        left.push(bold(" BLOCKING", t.need));
    }
    if a.dialog {
        left.push(bold(" HUB", t.remote));
    }
    let right = vec![
        Span::raw(" "),
        place(t, g, &a.host, &a.pane_id),
        sp(format!("{:>4}", age(a.age_ms)), t.sub),
    ];
    vec![
        ui::row(t, ui::spread(left, right, body), on, false, t.need, width),
        ui::row(t, second(&a.text, t.sub, width), on, false, t.need, width),
    ]
}

fn ask_card(app: &App, width: usize, offset: usize) -> Card {
    let (t, g) = (&app.theme, &app.data.glance);
    let mut card = Card::new(
        app,
        "NEEDS YOU",
        g.needs_you.len().to_string(),
        t.need,
        offset,
    )
    .dim_if(true, app);
    (card.keep, card.unit, card.counted) = (2, ("ask", "asks"), true);
    card.route = "A lists all";
    for (i, a) in g.needs_you.iter().enumerate() {
        card.rows.push(Row {
            lines: ask_lines(app, a, app.row == offset + i, width),
            extra: None,
            once: false,
        });
    }
    if card.rows.is_empty() {
        let none = if app.filter.is_empty() {
            "Nothing needs you in this snapshot"
        } else {
            "No ask matches the filter"
        };
        card.head(none, t.sub, true, width);
    }
    card
}

fn check_card(app: &App, width: usize, offset: usize) -> Card {
    let (t, g) = (&app.theme, &app.data.glance);
    let body = width.saturating_sub(1);
    let mut card = Card::new(
        app,
        "TO CHECK",
        g.attention.len().to_string(),
        t.check,
        offset,
    )
    .dim_if(true, app);
    (card.unit, card.counted) = (("check", "checks"), true);
    for (i, a) in g.attention.iter().enumerate() {
        let on = app.row == offset + i;
        let left = vec![bold("! ", t.check), sp(check_name(g, a), t.text)];
        let host = if g.local(&a.host) {
            String::new()
        } else {
            ui::host_name(g, &a.host)
        };
        let right = vec![
            sp(format!(" {host}"), t.remote),
            sp(format!("{:>5}", age(a.age_ms)), t.sub),
        ];
        card.rows.push(Row {
            lines: vec![
                ui::row(t, ui::spread(left, right, body), on, false, t.check, width),
                ui::row(
                    t,
                    second(&a.text, t.check, width),
                    on,
                    false,
                    t.check,
                    width,
                ),
            ],
            extra: None,
            once: false,
        });
    }
    if card.rows.is_empty() {
        let none = if app.filter.is_empty() {
            "Nothing to check in this snapshot"
        } else {
            "No check matches the filter"
        };
        card.head(none, t.sub, true, width);
    }
    card
}

fn campaign_card(app: &App, width: usize, offset: usize) -> Card {
    let (t, g) = (&app.theme, &app.data.glance);
    let body = width.saturating_sub(1);
    // Every listed campaign, parked ones too: the Fleet tab counts the same.
    let mut card = Card::new(
        app,
        "CAMPAIGNS",
        g.campaigns.len().to_string(),
        t.sub,
        offset,
    )
    .dim_if(true, app);
    (card.unit, card.route) = (("listed", "listed"), "F");
    let active: Vec<&CampaignRow> = g.campaigns.iter().filter(|c| !c.parked).collect();
    // mark, name, seven pips, the sparkline, then ten columns for the host and the age.
    // Under 40 columns the sparkline goes first (V2).
    let name_w = if body >= 53 {
        20
    } else {
        18.min(body.saturating_sub(21))
    };
    // No bars at all (a taskr older than P1b): the names take the room.
    let spark_w = if width < 40 || active.iter().all(|c| c.spark.is_empty()) {
        0
    } else {
        body.saturating_sub(name_w + 20).clamp(6, 16)
    };
    for c in &active {
        let on = app.row == offset + card.rows.len();
        let mut left = vec![
            ui::lead_mark(t, &c.lead, c.lead_waiting),
            Span::raw(" "),
            sp(pad(&c.name, name_w), t.text),
        ];
        left.extend(ui::pips(t, &c.lanes, 7));
        let host = if g.local(&c.host) {
            String::new()
        } else {
            ui::host_name(g, &c.host)
        };
        let mut right = vec![
            sp(format!(" {host} "), t.remote),
            sp(format!("{:>3}", age(c.activity_age_ms)), t.sub),
        ];
        if let Some(label) = c.tmp_label()
            && ui::width(&right) + label.len() < spark_w + 10
            && ui::width(&left) + 1 + ui::width(&right) + label.len() < body
        {
            right.insert(0, sp(format!(" {label}"), t.dim));
        }
        // A long machine name takes its room from the sparkline.
        let bars = spark_w.saturating_sub(ui::width(&right).saturating_sub(10));
        left.push(sp(format!(" {}", ui::spark(&c.spark, bars)), t.accent));
        let changed = app.changed.contains(&c.id);
        card.rows.push(Row {
            lines: vec![ui::row(
                t,
                ui::spread(left, right, body),
                on,
                changed,
                t.accent,
                width,
            )],
            extra: Some((
                c.activity_age_ms,
                ui::row(
                    t,
                    second(c.status_text(), t.dim, width),
                    on,
                    false,
                    t.accent,
                    width,
                ),
            )),
            once: false,
        });
    }
    for c in g.campaigns.iter().filter(|c| c.parked) {
        let left = vec![sp("‖ ", t.dim), sp(c.name.clone(), t.dim)];
        let mut right = vec![sp(format!(" parked {:>3}", age(c.park_age_ms)), t.dim)];
        if let Some(label) = c.tmp_label()
            && ui::width(&left) + ui::width(&right) + label.len() < body
        {
            right.insert(0, sp(format!(" {label}"), t.dim));
        }
        let on = app.row == offset + card.rows.len();
        card.rows.push(Row {
            lines: vec![ui::row(
                t,
                ui::spread(left, right, body),
                on,
                false,
                t.accent,
                width,
            )],
            extra: None,
            once: false,
        });
    }
    // Quiet roots are counted by the snapshot, never listed: only `c` lists them.
    if g.quiet.count > 0 {
        card.tail = Some(Line::from(sp(format!(" · {} quiet", g.quiet.count), t.dim)));
        card.note = format!(" · {} quiet unlisted · c", g.quiet.count);
    }
    if card.rows.is_empty() {
        let none = if app.filter.is_empty() {
            "No listed campaigns; not a fleet-wide idle claim"
        } else {
            "No campaign matches the filter"
        };
        card.head(none, t.sub, true, width);
    }
    card
}

/// Failed deploys, new Sentry issues and a stale read take the attention colour, never red;
/// pending and unknown stay neutral.
fn health_color(app: &App) -> Color {
    if app.health.attention() || app.health.error.is_some() {
        app.theme.check
    } else {
        app.theme.sub
    }
}

/// Health: which read it is, the one informational row, then the report's own clocks. Each
/// wraps rather than clips, so a narrow pane still reaches every value.
fn health_card(app: &App, width: usize, offset: usize) -> Card {
    let (t, h) = (&app.theme, &app.health);
    let color = health_color(app);
    let mut card = Card::new(
        app,
        "HEALTH",
        "hub report · production".into(),
        t.sub,
        offset,
    );
    if h.repo.is_none() {
        card.head(crate::config::HEALTH_HINT, t.sub, true, width);
        return card;
    }
    let state = if h.error.is_some() { t.check } else { t.sub };
    card.head(h.state(), state, true, width);
    let on = app.row == offset;
    card.rows.push(Row {
        lines: wrap(&h.label(app.observed_at), width.saturating_sub(1))
            .into_iter()
            .map(|line| ui::row(t, vec![sp(line, color)], on, false, color, width))
            .collect(),
        extra: None,
        once: true,
    });
    card.foot = wrap(&h.source(), width)
        .into_iter()
        .map(|line| Line::from(sp(line, t.dim)))
        .collect();
    card
}

const SELECT: &str = "Select a campaign and press l; Esc returns here";

/// A line of text, its colour, and whether it stays at every height.
type Text = (String, Color, bool);

/// A ledger panel's lines under its rule, then its rows, each with its colour. Every line
/// says what it is drawn from: the campaign read on hand and its age, or the glance's
/// listed campaigns; unknown stays unknown.
fn cached_parts(app: &App, panel: Panel) -> (Vec<Text>, Vec<(String, Color)>) {
    let (t, g) = (&app.theme, &app.data.glance);
    let mut head = vec![];
    if panel == Panel::Fleet {
        let fleet = &app.fleet;
        if !app.fetch.loaded {
            let why = if app.fetch.error.is_some() {
                "Ledger unreachable; counts unknown"
            } else {
                "Ledger not read yet; counts unknown"
            };
            return (vec![(why.into(), t.sub, true)], vec![]);
        }
        // Totals of what was reported; `+?` where some campaign did not say.
        let total = |i: usize| {
            let seen = fleet.iter().map(|c| c.lanes.observed()[i]);
            let sum: u32 = seen.clone().flatten().sum();
            if seen.clone().any(|n| n.is_none()) {
                format!("{sum}+?")
            } else {
                sum.to_string()
            }
        };
        head.push((
            format!(
                "{}{} listed · {} working {} ready {} open",
                if app.filter.is_empty() {
                    ""
                } else {
                    "filtered: "
                },
                fleet.len(),
                total(0),
                total(1),
                total(2)
            ),
            t.sub,
            false,
        ));
        if g.quiet.count > 0 {
            head.push((
                format!("{} quiet not listed; no counts", g.quiet.count),
                t.dim,
                true,
            ));
        }
        if app.fetch.error.is_some() {
            head.push((
                format!("! ledger stale · as of {}", ui::hhmm(&g.now)),
                t.check,
                true,
            ));
        }
        if fleet.is_empty() {
            let none = if app.filter.is_empty() {
                "No listed campaigns; not a fleet-wide idle claim"
            } else {
                "No listed campaign matches the filter"
            };
            head.push((none.into(), t.sub, true));
        }
        let rows = fleet
            .iter()
            .map(|c| {
                let [w, r, o] = c
                    .lanes
                    .observed()
                    .map(|n| n.map_or("?".into(), |n| n.to_string()));
                let lead = if c.lead.is_empty() {
                    "?"
                } else {
                    c.lead.as_str()
                };
                let parked = if c.parked { " · parked" } else { "" };
                (
                    format!(
                        "{} · {w} working {r} ready {o} open · lead {lead}{parked}",
                        c.name
                    ),
                    t.text,
                )
            })
            .collect();
        return (head, rows);
    }
    let Some(c) = scoped(app) else {
        head.push((SELECT.into(), t.sub, true));
        if let Some(error) = &app.scope.error {
            head.push((format!("! {error}"), t.check, true));
        }
        return (head, vec![]);
    };
    let age = app.scope.age(app.observed_at).unwrap_or_default();
    head.push((
        format!("{} #{} · read {age} ago", c.root.name, c.root.id),
        t.text,
        true,
    ));
    if let Some(error) = &app.scope.error {
        head.push((
            format!("! {error} · showing the last good read"),
            t.check,
            true,
        ));
    }
    if panel == Panel::Decisions {
        head.push((
            c.goal.first().map_or_else(
                || "goal: none captured".into(),
                |goal| format!("goal: {}", one_line(goal)),
            ),
            t.sub,
            false,
        ));
        let doc = match c.docs.iter().find(|d| d.kind == "plan") {
            Some(d) if d.captured => format!(" · plan doc v{} captured", d.version),
            Some(d) => format!(" · plan doc v{} not captured", d.version),
            None => " · no plan doc listed".into(),
        };
        head.push((
            if c.plan.version == 0 {
                format!("plan: no version recorded{doc}")
            } else {
                format!(
                    "plan v{} · {} decisions, {} closed lanes since{doc}",
                    c.plan.version, c.plan.decisions_since, c.plan.closed_since
                )
            },
            t.sub,
            false,
        ));
        if c.decisions.is_empty() {
            head.push(("No decisions in force in this read".into(), t.sub, true));
        }
        let rows = c
            .decisions
            .iter()
            .map(|e| (format!("◆ #{} {}", e.id, one_line(&e.text)), t.text))
            .collect();
        return (head, rows);
    }
    head.push((
        "recent log sample (up to 100 events): decisions only".into(),
        t.sub,
        false,
    ));
    let rows: Vec<_> = logged(app)
        .into_iter()
        .map(|e| {
            (
                format!("#{} {} {}", e.id, ui::hhmm(&e.at), one_line(&e.text)),
                t.sub,
            )
        })
        .collect();
    if rows.is_empty() {
        head.push(("No decision events in this log sample".into(), t.sub, true));
    }
    (head, rows)
}

fn cached_card(app: &App, panel: Panel, width: usize, offset: usize) -> Card {
    let t = &app.theme;
    let (title, right, unit) = match panel {
        Panel::Decisions => ("DECISIONS · PLAN", "cached read", ("decision", "decisions")),
        Panel::Fleet => ("FLEET", "listed campaigns", ("campaign", "campaigns")),
        _ => ("DECISION LOG", "recent sample", ("decision", "decisions")),
    };
    let mut card =
        Card::new(app, title, right.into(), t.sub, offset).dim_if(panel == Panel::Fleet, app);
    card.unit = unit;
    if panel == Panel::Fleet {
        card.route = "F lists all";
    }
    let (head, rows) = cached_parts(app, panel);
    for (text, color, keep) in head {
        card.head(text, color, keep, width);
    }
    for (i, (label, color)) in rows.into_iter().enumerate() {
        card.rows.push(Row {
            lines: vec![ui::row(
                t,
                vec![sp(clip(&label, width.saturating_sub(1)), color)],
                app.row == offset + i,
                false,
                color,
                width,
            )],
            extra: None,
            once: false,
        });
    }
    card
}

fn observed_card(app: &App, kind: Kind, width: usize, offset: usize) -> Card {
    let t = &app.theme;
    let d = kind.dashboard(app);
    let groups = d.authors(app.observed_at);
    let runs = d.runs();
    let unavailable = d.sources.iter().all(|s| s.at.is_none());
    let (title, right, unit) = match kind {
        Kind::Shipped => ("SHIPPED", "observed", ("author", "authors")),
        Kind::Issues => (
            "ISSUES",
            app.config.issues_repo.as_deref().unwrap_or_default(),
            ("group", "groups"),
        ),
        _ => ("BUILD QUEUE", "observed", ("run", "runs")),
    };
    let mut card = Card::new(app, title, right.into(), t.sub, offset);
    card.unit = unit;
    if d.sources.is_empty() {
        let hint = if kind == Kind::Issues {
            crate::config::ISSUES_HINT
        } else {
            crate::config::REPOS_HINT
        };
        card.head(hint, t.sub, true, width);
        return card;
    }
    let summary = if unavailable {
        "Counts unavailable".into()
    } else {
        match kind {
            Kind::Shipped => format!(
                "{} observed merged / rolling 24h",
                groups.values().map(Vec::len).sum::<usize>()
            ),
            Kind::Issues => {
                let [open, blocked, closed] = d.issue_totals(app.observed_at);
                format!("{open} open · {blocked} blocked · {closed} closed/24h · unique")
            }
            _ => format!(
                "{} active observed · {} unknown",
                runs.iter().filter(|r| r.known()).count(),
                runs.iter().filter(|r| !r.known()).count()
            ),
        }
    };
    card.head(summary, t.sub, unavailable, width);
    for s in &d.sources {
        // Each source's status (its read's age, or stale/unavailable) is kept at the floor.
        let color = if s.error.is_some() { t.check } else { t.text };
        card.head(s.status(app.observed_tick), color, true, width);
        let capped = match kind {
            Kind::Shipped => vec![(s.total == 100).then_some("first 100; more may exist")],
            Kind::Issues => vec![
                (s.total == 100).then_some("open: first 100; more may exist"),
                (s.closed_total == 100).then_some("closed: first 100; more may exist"),
            ],
            _ => vec![],
        };
        for text in capped.into_iter().flatten() {
            card.head(text, t.check, true, width);
        }
    }
    let note = match kind {
        Kind::Build => Some("latest100-run sample · older may be missing"),
        Kind::Issues => Some("current assignees · overlap · non-atomic"),
        _ => None,
    };
    // The Build sample limit is a cap, kept like "first 100"; the Issues note only explains.
    if let Some(note) = note {
        card.head(note, t.sub, kind == Kind::Build, width);
    }
    let stale = |repo: Option<&str>| {
        d.sources
            .iter()
            .any(|s| repo.is_none_or(|r| s.repo == r) && s.error.is_some())
    };
    // Label, second line, stale, colour. Blocked issues take the attention colour, never red.
    let labels: Vec<(String, Option<String>, bool, Color)> = match kind {
        Kind::Shipped => groups
            .iter()
            .map(|(a, rows)| {
                (
                    format!("@{a} · {} PRs observed", rows.len()),
                    None,
                    stale(None),
                    t.sub,
                )
            })
            .collect(),
        Kind::Issues => d
            .assignees(app.observed_at)
            .iter()
            .map(|g| {
                (
                    format!("{} · {}", g.name(), g.counts()),
                    None,
                    stale(Some(g.repo.as_str())),
                    if g.blocked > 0 { t.check } else { t.sub },
                )
            })
            .collect(),
        _ => runs
            .iter()
            .map(|r| {
                (
                    format!("{} · {}", r.label(), r.workflow),
                    Some(format!(
                        "  {} · {} · {}",
                        r.branch,
                        r.created.filter(|at| *at <= app.observed_at).map_or_else(
                            || "age unknown".into(),
                            |at| age((app.observed_at - at).saturating_mul(1000))
                        ),
                        r.repo
                    )),
                    stale(Some(r.repo.as_str())),
                    t.sub,
                )
            })
            .collect(),
    };
    if labels.is_empty() && !unavailable {
        let none = match kind {
            Kind::Shipped => "None observed in cached 24h sample",
            Kind::Issues => "No open or 24h-closed issues observed",
            _ => "None active observed in latest100 sample",
        };
        card.head(none, t.sub, true, width);
    }
    for (i, (label, meta, stale, color)) in labels.into_iter().enumerate() {
        let on = app.row == offset + i;
        // The stale cue first: a narrow row clips its end.
        let label = format!("{}{label}", if stale { "stale/partial · " } else { "" });
        let mut lines = vec![ui::row(
            t,
            vec![sp(clip(&label, width.saturating_sub(1)), color)],
            on,
            false,
            color,
            width,
        )];
        if let Some(meta) = meta {
            lines.push(ui::row(
                t,
                vec![sp(clip(&meta, width.saturating_sub(1)), t.sub)],
                on,
                false,
                t.sub,
                width,
            ));
        }
        card.rows.push(Row {
            lines,
            extra: None,
            once: false,
        });
    }
    card
}

/// My PRs (panel 4): one row per observed PR. Wide: the title and the compact facts on one
/// line; mid and narrow: two. `I` shows every field. Failing checks are the card's only red.
fn pr_card(app: &App, width: usize, offset: usize) -> Card {
    use crate::prs::Ci;
    let t = &app.theme;
    let mine = &app.mine;
    let right = mine
        .login
        .as_ref()
        .map_or(String::new(), |l| format!("@{l}"));
    let mut card = Card::new(app, "MY PRS", right, t.sub, offset);
    card.unit = ("PR", "PRs");
    if mine.repos.is_empty() {
        card.head(crate::config::REPOS_HINT, t.sub, true, width);
        return card;
    }
    let color = if mine.error.is_some() {
        t.check
    } else {
        t.text
    };
    card.head(mine.status(app.observed_tick), color, true, width);
    for (note, keep) in mine.notes() {
        card.head(note, t.check, keep, width);
    }
    if mine.at.is_some() && mine.prs.is_empty() {
        let none = if app.filter.is_empty() {
            "0 observed in search sample"
        } else {
            "No PR matches the filter"
        };
        card.head(none, t.sub, true, width);
    } else if mine.at.is_some() {
        // Spare lines of whole items, so a narrow card wraps the legend instead of cutting
        // it and `req ✗` never parts from its rule; help (`?`) has it all at any size.
        let mut line = String::new();
        for item in [
            "? not known",
            "N+? at least N, more unread",
            "req ✗ only from an observed required failure",
            "I every field",
        ] {
            let joined = format!("{line} · {item}");
            if line.is_empty() {
                line = item.into();
            } else if joined.width() > width.saturating_sub(2) {
                card.head(
                    std::mem::replace(&mut line, item.into()),
                    t.dim,
                    false,
                    width,
                );
            } else {
                line = joined;
            }
        }
        card.head(line, t.dim, false, width);
    }
    let several = mine.repos.len() > 1;
    let now = app.observed_at;
    for (i, p) in mine.prs.iter().enumerate() {
        let on = app.row == offset + i;
        let ci = p.ci();
        let color = match ci {
            Ci::Failing => t.need,
            Ci::Running => t.check,
            Ci::Passing => t.ok,
            Ci::Unknown | Ci::Draft => t.sub,
        };
        let name = p.repo.rsplit('/').next().unwrap_or(&p.repo);
        let id = if several {
            format!("{name}#{}", p.number)
        } else {
            format!("#{}", p.number)
        };
        let title = match &p.read {
            Some(f) if f.draft => format!("DRAFT {}", f.title),
            Some(f) => f.title.clone(),
            None => "not readable".into(),
        };
        let age = p
            .read
            .as_ref()
            .filter(|f| f.updated <= now)
            .map_or("?".into(), |f| {
                age(now.saturating_sub(f.updated).saturating_mul(1000))
            });
        let stale = if mine.error.is_some() {
            "stale · "
        } else {
            ""
        };
        let facts = format!("{stale}{}", p.facts());
        let head = vec![
            sp(format!("{} ", ci.mark()), color),
            sp(format!("{id} "), t.text),
            sp(title, t.text),
        ];
        let lines = if width >= 100 {
            // The facts and age keep up to three fifths of the line; the title gives way
            // first, then the facts' end (failing names), never the age.
            let age = format!("  {age}");
            let facts = clip(
                &format!("  {facts}"),
                ((width - 1) * 3 / 5).saturating_sub(age.width()),
            );
            vec![ui::row(
                t,
                ui::spread(head, vec![sp(facts + &age, t.sub)], width - 1),
                on,
                false,
                color,
                width,
            )]
        } else {
            vec![
                ui::row(
                    t,
                    ui::spread(
                        head,
                        vec![sp(format!(" {age}"), t.sub)],
                        width.saturating_sub(1),
                    ),
                    on,
                    false,
                    color,
                    width,
                ),
                ui::row(
                    t,
                    vec![sp(
                        clip(&format!("  {facts}"), width.saturating_sub(1)),
                        t.sub,
                    )],
                    on,
                    false,
                    color,
                    width,
                ),
            ]
        };
        card.rows.push(Row {
            lines,
            extra: None,
            once: false,
        });
    }
    card
}

/// The cards in band order (proposal §4.2), each with its band and side (`true`: right).
/// A: needs you │ to check. B: PRs, full width. C: campaigns │ Shipped, Build. D: Decisions,
/// Decision log │ Fleet, Issues, Health. A band with one side shown gives it the full width.
/// Slot, band, right side, shown: the Dashboard's eleven cards in band order. The asks,
/// checks and campaigns give way to the ledger's state while it has none of them.
fn slots(app: &App) -> [(usize, usize, bool, bool); 11] {
    let p = &app.config.panels;
    let ledger = !ledger_state(app);
    [
        (0, 0, false, ledger && p[0]),
        (1, 0, true, ledger && p[1]),
        (
            2,
            1,
            false,
            p[3] && (app.config.repos.is_empty() || !app.mine.repos.is_empty()),
        ),
        (3, 2, false, ledger && p[2]),
        (4, 2, true, github_shown(app, Kind::Shipped)),
        (5, 2, true, github_shown(app, Kind::Build)),
        (6, 3, false, p[6]),
        (7, 3, false, p[8]),
        (8, 3, true, p[7]),
        (9, 3, true, github_shown(app, Kind::Issues)),
        (10, 3, true, p[10]),
    ]
}

/// No asks, checks or campaigns, listed or quiet: the state (loading, unreachable, empty)
/// stands in their place.
fn ledger_state(app: &App) -> bool {
    let g = &app.data.glance;
    g.needs_you.is_empty() && g.attention.is_empty() && g.campaigns.is_empty() && g.quiet.count == 0
}

fn board(app: &App, width: usize, two: bool) -> Vec<(usize, bool, Card)> {
    let slots = slots(app);
    let split = |band: usize| {
        let side = |right: bool| slots.iter().any(|s| s.1 == band && s.2 == right && s.3);
        two && side(false) && side(true)
    };
    let mut offset = 0;
    let mut out = vec![];
    for (slot, band, right, shown) in slots {
        if !shown {
            continue;
        }
        let w = match (split(band), right) {
            (false, _) => width,
            (true, false) => width.div_ceil(2),
            (true, true) => width / 2,
        };
        let card = match slot {
            0 => ask_card(app, w, offset),
            1 => check_card(app, w, offset),
            2 => pr_card(app, w, offset),
            3 => campaign_card(app, w, offset),
            4 => observed_card(app, Kind::Shipped, w, offset),
            5 => observed_card(app, Kind::Build, w, offset),
            6 => cached_card(app, Panel::Decisions, w, offset),
            7 => cached_card(app, Panel::Log, w, offset),
            8 => cached_card(app, Panel::Fleet, w, offset),
            9 => observed_card(app, Kind::Issues, w, offset),
            _ => health_card(app, w, offset),
        };
        offset += card.rows.len().max(1);
        out.push((band, right && split(band), card));
    }
    out
}

/// Card heights for `body` rows (proposal §4.3): every card's want; else, while the floors
/// fit, cards give rows back from Health up to the asks and spare rows return in band order;
/// else every floor, for the viewport. A band is as tall as its taller side.
fn heights(cards: &[(usize, bool, Card)], body: usize, row: usize) -> Vec<usize> {
    let total = |h: &[usize]| {
        let mut bands = [[0usize; 2]; 4];
        for ((band, right, _), h) in cards.iter().zip(h) {
            bands[*band][usize::from(*right)] += h;
        }
        bands.iter().map(|[l, r]| *l.max(r)).sum::<usize>()
    };
    let want: Vec<usize> = cards.iter().map(|c| c.2.want()).collect();
    let full: Vec<usize> = cards.iter().map(|c| c.2.full()).collect();
    let floor: Vec<usize> = cards
        .iter()
        .map(|c| c.2.floor(row).min(c.2.want()))
        .collect();
    // Floors that overflow make a viewport; a split band's shorter side still fills the
    // rows its taller side already takes.
    let body = body.max(total(&floor));
    let mut h = want.clone();
    for i in (0..h.len()).rev() {
        while total(&h) > body && h[i] > floor[i] {
            h[i] -= 1;
        }
    }
    // Back in band order: first to the cut cards' wants, then to second lines.
    for goal in [&want, &full] {
        for i in 0..h.len() {
            while h[i] < goal[i] {
                h[i] += 1;
                if total(&h) > body {
                    h[i] -= 1;
                    break;
                }
            }
        }
    }
    h
}

/// A card placed in the body: its column, first line and lines.
struct Block {
    x: u16,
    width: u16,
    y: usize,
    drawn: Drawn,
    brief: String,
}

/// The cards laid out at `width`: tiny one-line cards, else fitted cards band by band.
fn blocks(app: &App, width: usize, height: usize, tiny: bool) -> Vec<Block> {
    let two = width >= 80 && !tiny;
    let cards = board(app, width, two);
    let h = if tiny {
        vec![1; cards.len()]
    } else {
        heights(&cards, height, app.row)
    };
    let each = app.screen != Screen::Glance;
    let mut out = vec![];
    let mut y = 0;
    let mut band = (usize::MAX, [0usize; 2]);
    for ((b, right, card), h) in cards.iter().zip(h) {
        if *b != band.0 {
            y += band.1[0].max(band.1[1]);
            band = (*b, [0, 0]);
        }
        let w = match (*right, cards.iter().any(|c| c.0 == *b && c.1)) {
            (true, _) => width / 2,
            (false, true) => width.div_ceil(2),
            (false, false) => width,
        };
        let drawn = if tiny {
            card.tiny(app, w, each || card.title == "NEEDS YOU")
        } else {
            card.fit(&app.theme, h, app.row, w)
        };
        let side = usize::from(*right);
        let x = if *right { width.div_ceil(2) } else { 0 };
        let len = drawn.lines.len();
        out.push(Block {
            x: x as u16,
            width: w as u16,
            y: y + band.1[side],
            drawn,
            brief: card.brief(),
        });
        band.1[side] += len;
    }
    out
}

/// The first content line the viewport shows (proposal §4.4): the smallest whole-card
/// offset that keeps the cursor's whole card in view, else its rule and row, else the
/// cursor row itself.
/// The disclosure lines are reserved first.
fn viewport(blocks: &[Block], total: usize, body: usize, row: usize) -> usize {
    if total <= body {
        return 0;
    }
    let avail = |s: usize| {
        let h = body.saturating_sub(usize::from(s > 0));
        if s + h < total {
            h.saturating_sub(1)
        } else {
            h
        }
    };
    let Some(at) = blocks
        .iter()
        .position(|b| b.drawn.hits.contains(&Some(row)))
    else {
        return 0;
    };
    let b = &blocks[at];
    let first = b.y;
    let last = b.y
        + b.drawn
            .hits
            .iter()
            .rposition(|h| *h == Some(row))
            .unwrap_or(0);
    // The cursor's whole card where it fits (its floor and notices), else down to its row.
    let end = b.y + b.drawn.lines.len().max(1) - 1;
    let starts = |last: usize| {
        blocks
            .iter()
            .map(|b| b.y)
            .filter(|s| *s <= first && last < s + avail(*s))
            .min()
    };
    starts(end)
        .or_else(|| starts(last))
        .or_else(|| (0..=last).find(|s| last < s + avail(*s)))
        .unwrap_or(last)
}

/// The Dashboard body: its cards, as many as fit, scrolled to keep the cursor row and its
/// card's rule drawn. Only drawn rows are hit targets; the disclosure lines are tab and
/// shift-tab.
fn body(f: &mut Frame, app: &App, area: Rect, tiny: bool) {
    let t = &app.theme;
    let width = area.width as usize;
    let height = area.height as usize;
    let blocks = blocks(app, width, height, tiny);
    let total = blocks
        .iter()
        .map(|b| b.y + b.drawn.lines.len())
        .max()
        .unwrap_or(0);
    let top = viewport(&blocks, total, height, app.row);
    let up = usize::from(top > 0);
    let view = if total <= height {
        height
    } else {
        let h = height.saturating_sub(up);
        if top + h < total {
            h.saturating_sub(1)
        } else {
            h
        }
    };
    let dashboard = app.screen == Screen::Glance;
    let mut cue = |y: usize, text: String, key: &str| {
        let line = Rect {
            y: area.y + y as u16,
            height: 1,
            ..area
        };
        f.render_widget(
            Paragraph::new(Line::from(sp(clip(&text, width), t.dim))),
            line,
        );
        if dashboard {
            app.hit(line, Hit::Key(key.into()));
        }
    };
    if up > 0 {
        let above: Vec<String> = blocks
            .iter()
            .filter(|b| b.y < top)
            .map(|b| b.brief.clone())
            .collect();
        cue(
            0,
            format!(" ↑ {} above: {}", above.len(), above.join(" · ")),
            "shift-tab",
        );
    }
    let end = top + view;
    let below: Vec<String> = blocks
        .iter()
        .filter(|b| b.y + b.drawn.lines.len() > end)
        .map(|b| b.brief.clone())
        .collect();
    if !below.is_empty() {
        let n = below.len();
        let cards = if n == 1 { "card" } else { "cards" };
        cue(
            up + view,
            format!(" ↓ {n} {cards} below: {} · tab", below.join(" · ")),
            "tab",
        );
    }
    for b in &blocks {
        let from = top.max(b.y);
        let to = end.min(b.y + b.drawn.lines.len());
        // A card is drawn whole or, named by a disclosure line, not at all: never a bare
        // rule over rows out of view. Only the cursor's own card may be cut, around the row.
        let whole = from == b.y && to == b.y + b.drawn.lines.len();
        if from >= to || !(whole || b.drawn.hits.contains(&Some(app.row))) {
            continue;
        }
        let rect = Rect {
            x: area.x + b.x,
            y: area.y + (up + from - top) as u16,
            width: b.width,
            height: (to - from) as u16,
        };
        let lines = b.drawn.lines[from - b.y..to - b.y].to_vec();
        for (i, hit) in b.drawn.hits[from - b.y..to - b.y].iter().enumerate() {
            if let Some(row) = hit {
                app.hit(
                    Rect {
                        y: rect.y + i as u16,
                        height: 1,
                        ..rect
                    },
                    Hit::Row(*row),
                );
            }
        }
        f.render_widget(Paragraph::new(lines), rect);
    }
}

#[cfg(test)]
pub(crate) fn lines(app: &App, width: usize, height: usize) -> Vec<Line<'static>> {
    let mut out = vec![];
    for b in blocks(app, width, height, false) {
        out.extend(b.drawn.lines);
    }
    out
}

/// One line for row `i`, as the tiny frame lists it: what it is, then its text.
fn label(app: &App, i: usize) -> Option<(String, Color)> {
    let mut at = app.clone();
    at.row = i;
    let stale = |d: &crate::github::Dashboard| d.sources.iter().any(|s| s.error.is_some());
    let (text, color) = match selected(&at) {
        Selected::Ask(a) => (
            format!("ask {} {}", a.campaign, one_line(&a.text)),
            app.theme.need,
        ),
        Selected::Check(a) => (
            format!(
                "check {} {}",
                check_name(&app.data.glance, a),
                one_line(&a.text)
            ),
            app.theme.check,
        ),
        Selected::Campaign(c) => (
            format!("campaign {} {}", c.name, c.status_text()),
            app.theme.text,
        ),
        Selected::Pr(p) => (
            format!(
                "PR{} {} #{} {} · {}",
                if app.mine.error.is_some() {
                    " stale"
                } else {
                    ""
                },
                p.repo,
                p.number,
                p.read.as_ref().map_or("not readable", |f| f.title.as_str()),
                p.facts()
            ),
            app.theme.sub,
        ),
        Selected::Author(author) => {
            let groups = app.shipped.authors(app.observed_at);
            (
                format!(
                    "Shipped @{} · {} observed / 24h{}",
                    author,
                    groups[&author].len(),
                    if stale(&app.shipped) {
                        " · stale/partial"
                    } else {
                        ""
                    }
                ),
                app.theme.sub,
            )
        }
        Selected::Run(r) => (
            format!(
                "Build{} {} {} · {} · latest100 sample",
                if stale(&app.builds) { " stale" } else { "" },
                r.label(),
                r.workflow,
                r.branch
            ),
            app.theme.sub,
        ),
        Selected::Decision(e) => (
            format!("decision #{} {}", e.id, one_line(&e.text)),
            app.theme.text,
        ),
        Selected::Logged(e) => (
            format!(
                "logged decision #{} {} {}",
                e.id,
                ui::hhmm(&e.at),
                one_line(&e.text)
            ),
            app.theme.sub,
        ),
        Selected::Fleet(c) => {
            let [w, r, o] = c
                .lanes
                .observed()
                .map(|n| n.map_or("?".into(), |n| n.to_string()));
            (
                format!("fleet {} · {w} working {r} ready {o} open", c.name),
                app.theme.text,
            )
        }
        Selected::Health => (
            // Stale, unknown or "as of" comes first: a narrow row clips its end.
            format!("Health · {}", app.health.label(app.observed_at)),
            health_color(app),
        ),
        Selected::Assignee(g) => (
            // The stale cue comes first: a narrow row clips its end.
            format!(
                "Issues{} {} · {}",
                if stale(&app.issues) { " stale" } else { "" },
                g.name(),
                g.counts()
            ),
            if g.blocked > 0 {
                app.theme.check
            } else {
                app.theme.sub
            },
        ),
        Selected::Nothing => return None,
    };
    Some((text, color))
}

/// A row's identity, so the cursor stays on it when a snapshot moves it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RowId {
    Ask(i64),
    Check(i64, String),
    Campaign(i64),
    Pr(String, u64),
    Author(String),
    Run(String, u64),
    /// A decision in force in the campaign read on hand: root, event.
    Decision(i64, i64),
    /// A listed campaign's Fleet row: apart from its campaign row.
    Fleet(i64),
    /// A decision event in that read's log sample: apart from the same event in force.
    Logged(i64, i64),
    /// An Issues group: repository and raw login; None is Unassigned, apart from any login.
    Assignee(String, Option<String>),
    /// Health's row: its configured repository and environment, never the note it shows.
    Health(String, &'static str),
    /// An empty or unavailable card's state, by its slot: a place to look, never a target.
    Card(usize),
}

/// Every row the cursor can be on, in cursor order: the cards' rows in band order, and an
/// empty card's one inert stop.
pub(crate) fn ids(app: &App) -> Vec<RowId> {
    let g = &app.data.glance;
    let root = app.data.campaign.root.id;
    let mut out = vec![];
    for (slot, .., shown) in slots(app) {
        if !shown {
            continue;
        }
        let rows: Vec<RowId> = match slot {
            0 => g.needs_you.iter().map(|a| RowId::Ask(a.ask_id)).collect(),
            1 => g
                .attention
                .iter()
                .map(|a| RowId::Check(a.root_id, a.kind.clone()))
                .collect(),
            2 => app
                .mine
                .prs
                .iter()
                .map(|p| RowId::Pr(p.repo.clone(), p.number))
                .collect(),
            3 => g
                .campaigns
                .iter()
                .filter(|c| !c.parked)
                .chain(g.campaigns.iter().filter(|c| c.parked))
                .map(|c| RowId::Campaign(c.id))
                .collect(),
            4 => app
                .shipped
                .authors(app.observed_at)
                .into_keys()
                .map(RowId::Author)
                .collect(),
            5 => app
                .builds
                .runs()
                .into_iter()
                .map(|r| RowId::Run(r.repo.clone(), r.id))
                .collect(),
            6 => decisions(app)
                .iter()
                .map(|e| RowId::Decision(root, e.id))
                .collect(),
            7 => logged(app)
                .into_iter()
                .map(|e| RowId::Logged(root, e.id))
                .collect(),
            8 => app.fleet.iter().map(|c| RowId::Fleet(c.id)).collect(),
            9 => app
                .issues
                .assignees(app.observed_at)
                .into_iter()
                .map(|g| RowId::Assignee(g.repo, g.login))
                .collect(),
            _ => app
                .health
                .repo
                .as_ref()
                .map(|repo| RowId::Health(repo.clone(), crate::health::ENV))
                .into_iter()
                .collect(),
        };
        if rows.is_empty() {
            out.push(RowId::Card(slot));
        }
        out.extend(rows);
    }
    out
}

/// The same visible identities drive Tab, keyboard, mouse and snapshot restoration. Each
/// empty card is a section of its own.
pub(crate) fn sections(app: &App) -> Vec<usize> {
    let mut out = vec![];
    let mut previous = None;
    for (i, row) in ids(app).iter().enumerate() {
        let kind = std::mem::discriminant(row);
        if previous != Some(kind) || matches!(row, RowId::Card(_)) {
            out.push(i);
        }
        previous = Some(kind);
    }
    out
}

/// The row under the cursor.
pub(crate) enum Selected<'a> {
    Ask(&'a Need),
    Check(&'a Attention),
    Campaign(&'a CampaignRow),
    Pr(&'a crate::prs::Pr),
    Author(String),
    Run(&'a crate::github::Run),
    Decision(&'a Event),
    Logged(&'a Event),
    Fleet(&'a CampaignRow),
    Assignee(crate::github::Group),
    Health,
    Nothing,
}

pub(crate) fn selected(app: &App) -> Selected<'_> {
    let g = &app.data.glance;
    match ids(app).get(app.row) {
        Some(RowId::Ask(id)) => g
            .needs_you
            .iter()
            .find(|a| a.ask_id == *id)
            .map_or(Selected::Nothing, Selected::Ask),
        Some(RowId::Check(root, kind)) => g
            .attention
            .iter()
            .find(|a| a.root_id == *root && a.kind == *kind)
            .map_or(Selected::Nothing, Selected::Check),
        Some(RowId::Campaign(id)) => g
            .campaigns
            .iter()
            .find(|c| c.id == *id)
            .map_or(Selected::Nothing, Selected::Campaign),
        Some(RowId::Pr(repo, number)) => app
            .mine
            .prs
            .iter()
            .find(|p| p.repo == *repo && p.number == *number)
            .map_or(Selected::Nothing, Selected::Pr),
        Some(RowId::Author(author)) => Selected::Author(author.clone()),
        Some(RowId::Run(repo, id)) => app
            .builds
            .runs()
            .into_iter()
            .find(|r| r.repo == *repo && r.id == *id)
            .map_or(Selected::Nothing, Selected::Run),
        Some(RowId::Decision(_, id)) => decisions(app)
            .iter()
            .find(|e| e.id == *id)
            .map_or(Selected::Nothing, Selected::Decision),
        Some(RowId::Logged(_, id)) => logged(app)
            .into_iter()
            .find(|e| e.id == *id)
            .map_or(Selected::Nothing, Selected::Logged),
        Some(RowId::Fleet(id)) => app
            .fleet
            .iter()
            .find(|c| c.id == *id)
            .map_or(Selected::Nothing, Selected::Fleet),
        Some(RowId::Assignee(repo, login)) => app
            .issues
            .assignees(app.observed_at)
            .into_iter()
            .find(|g| g.repo == *repo && g.login == *login)
            .map_or(Selected::Nothing, Selected::Assignee),
        Some(RowId::Health(..)) => Selected::Health,
        Some(RowId::Card(_)) | None => Selected::Nothing,
    }
}

/// The footer lists only the keys that do something for the selected row.
pub(crate) fn hints(app: &App, wide: bool) -> Vec<(&'static str, &'static str)> {
    let g = &app.data.glance;
    let detail = app.screen == Screen::Detail;
    // `⏎` is offered only where there is a pane to go to.
    let go = |host: &str, pane: &str| match (pane.is_empty(), g.local(host), wide) {
        (true, ..) => None,
        (_, false, _) => Some(hint("⏎", Some("how to get there"))),
        (_, true, true) => Some(hint("⏎", Some("go to lead"))),
        (_, true, false) => Some(hint("⏎", None)),
    };
    let (page, full) = {
        let seen = app.seen.borrow();
        (seen.page, seen.full)
    };
    let asks = app.screen == Screen::Asks;
    let mut out: Vec<_> = match selected(app) {
        // A narrow Asks screen of several asks reads the selected one in full on `⏎`.
        Selected::Ask(_) if asks && !full => vec![
            Some(hint("⏎", Some("read in full"))),
            Some(hint("a", None)),
            Some(hint("l", None)),
        ],
        Selected::Ask(_) if asks => vec![
            Some(hint("⏎", Some("answer"))),
            Some(hint("a", None)),
            Some(hint("l", None)),
        ],
        Selected::Ask(a) => vec![
            go(&a.host, &a.pane_id),
            Some(hint("a", None)),
            Some(hint("l", None)),
        ],
        Selected::Check(a) => vec![
            go(&a.host, &a.pane_id),
            (a.root_id != 0).then(|| hint("l", None)),
        ],
        // Fleet's `l` also fills the lanes under its roster; `c` lists the closed ones too.
        Selected::Campaign(c) if app.screen == Screen::Fleet && wide => vec![
            go(&c.host, &c.pane_id),
            Some(hint("l", Some("open (reads lanes)"))),
            Some(hint("p", None)),
            Some(hint("c", Some("closed too"))),
        ],
        Selected::Campaign(c) => vec![
            go(&c.host, &c.pane_id),
            Some(hint("l", None)),
            Some(hint("p", None)),
        ],
        // Detail admits no GitHub read, so `r` would not refresh what it shows.
        Selected::Pr(_) | Selected::Author(_) | Selected::Run(_) => {
            vec![(!detail).then(|| hint("r", None))]
        }
        Selected::Decision(_) | Selected::Logged(_) | Selected::Fleet(_) => {
            vec![Some(hint("o", Some("open campaign")))]
        }
        // Informational: traversal and help only.
        Selected::Assignee(_) | Selected::Health => vec![],
        Selected::Nothing if detail => vec![],
        Selected::Nothing => vec![Some(hint("c", Some("all campaigns")))],
    }
    .into_iter()
    .flatten()
    .collect();
    if app.fetch.error.is_some() {
        out.retain(|(key, _)| *key != "r");
        out.insert(0, hint("r", Some("retry")));
    }
    if detail {
        out.insert(0, hint("h", None));
    }
    if asks && full && page.1 > page.0 {
        out.insert(0, hint("ctrl-d ctrl-u", Some("scroll text")));
    }
    if wide {
        // Off an ask, `a` lists every open one to choose from.
        if !matches!(selected(app), Selected::Ask(_)) {
            out.push(hint("a", Some("asks")));
        }
        // `?` last: the footer keeps its last hint when the others give way.
        if app.screen == Screen::Glance {
            out.extend([hint("0-9", None), hint("H", None)]);
        }
        if app.screen.rows() {
            out.push(hint("/", None));
        }
        out.extend([
            hint("D A F I", None),
            hint("y", None),
            hint("s", None),
            hint("q", None),
            hint("?", None),
        ]);
    } else {
        out.extend([hint("D A F I", None), hint("?", None)]);
    }
    if matches!(
        selected(app),
        Selected::Pr(_)
            | Selected::Author(_)
            | Selected::Run(_)
            | Selected::Decision(_)
            | Selected::Logged(_)
            | Selected::Fleet(_)
            | Selected::Assignee(_)
            | Selected::Health
            | Selected::Nothing
    ) {
        out.retain(|(key, _)| *key != "y");
    }
    out
}

pub(crate) fn draw(f: &mut Frame, app: &App) {
    let (t, g) = (&app.theme, &app.data.glance);
    let area = f.area();
    let wide = area.width >= 100;
    let stale = app.fetch.error.as_deref().filter(|_| app.fetch.loaded);
    // The panel switches are the Dashboard's: Asks and Fleet list theirs whatever they say.
    let dashboard = !matches!(app.screen, Screen::Asks | Screen::Fleet);
    // Tiny (proposal §4.1): one-line cards, no panel strip or notes line.
    let tiny = area.width < 46 || area.height < 20;
    let toggle_lines = if dashboard && !tiny {
        // Short of rows, the switches take one compact line so the tab line costs no row.
        toggles(app, area.width as usize, wide, area.height < 32)
    } else {
        vec![]
    };
    let [head, tabs, switches, warning, notes, banner, body, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(toggle_lines.len() as u16),
        Constraint::Length(u16::from(app.config_warning.is_some())),
        Constraint::Length(u16::from(g.owner_notes_pending > 0 && !tiny)),
        Constraint::Length(u16::from(stale.is_some())),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);
    ui::header(
        f,
        app,
        head,
        if wide {
            vec![sp(app.screen.name().to_lowercase(), t.dim)]
        } else {
            vec![]
        },
    );
    ui::tabs(f, app, tabs);
    f.render_widget(Paragraph::new(toggle_lines), switches);
    if let Some(text) = &app.config_warning {
        f.render_widget(
            Paragraph::new(Line::from(sp(text.clone(), t.check))),
            warning,
        );
    }
    // The migration count: dim, never red, gone at zero.
    let pending = format!(" {} notes still carry OWNER items", g.owner_notes_pending);
    f.render_widget(Paragraph::new(Line::from(sp(pending, t.dim))), notes);
    if dashboard && !app.config.panels.iter().any(|p| *p) {
        ui::footer(f, app, foot, &hints(app, wide));
        f.render_widget(
            Paragraph::new("All panels hidden · press 1-9, 0 or H to show").centered(),
            body,
        );
        return;
    }
    // The asks, checks and campaigns give way to the ledger's state while it has none;
    // under other cards the state keeps only the lines it draws.
    let others = !ids(app).is_empty();
    let state_height = if ledger_state(app) {
        let lines = state_lines(app, body.width as usize).map_or(0, |(l, _)| l.len());
        if others {
            lines.min(body.height as usize / 2)
        } else {
            body.height as usize
        }
    } else {
        0
    };
    if state_height > 0 {
        state(
            f,
            app,
            Rect {
                height: state_height as u16,
                ..body
            },
        );
    }
    let mut cards = Rect {
        y: body.y + state_height as u16,
        height: body.height.saturating_sub(state_height as u16),
        ..body
    };
    // Every row is reachable: a card windows around the cursor, the body scrolls.
    app.seen.borrow_mut().rows = ids(app).len();
    // The Asks screen shows the selected ask in full: beside the list from 80 columns,
    // under it when it is the only one, else `⏎` or `I` reads it on Detail.
    // The answers sent from here, newest first, under the list.
    let sent = answered_here(app, cards.width);
    if app.screen == Screen::Asks && state_height == 0 && !sent.is_empty() {
        // Whole answers only, newest first, in at most half the room.
        let room = cards.height.saturating_sub(4) as usize / 2;
        let mut lines = vec![];
        for answer in sent {
            if lines.len() + answer.len() > room {
                break;
            }
            lines.extend(answer);
        }
        if !lines.is_empty() {
            let [list, below] = Layout::vertical([
                Constraint::Min(0),
                Constraint::Length(lines.len() as u16 + 1),
            ])
            .areas(cards);
            cards = list;
            lines.insert(0, Line::from(bold(" ANSWERED HERE", app.theme.sub)));
            f.render_widget(Paragraph::new(lines), below);
        }
    }
    let asks = g.needs_you.len();
    if app.screen == Screen::Asks && state_height == 0 && asks > 0 {
        if cards.width >= 80 {
            let list = if cards.width >= 100 { 46 } else { 40 };
            let [left, right] =
                Layout::horizontal([Constraint::Length(list), Constraint::Min(0)]).areas(cards);
            cards = left;
            ask_pane(f, app, right);
        } else if asks == 1 && cards.height > 6 {
            let [top, below] =
                Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(cards);
            cards = top;
            ask_pane(f, app, below);
        }
    }
    if app.screen == Screen::Fleet && state_height == 0 {
        crate::fleet::body(f, app, cards, tiny);
    } else {
        self::body(f, app, cards, tiny);
    }
    // After the body: the Asks screen's hints depend on what it drew.
    ui::footer(f, app, foot, &hints(app, wide));
    if let Some(error) = stale {
        // The last frame stays, dimmed, and says so.
        f.render_widget(Paragraph::new(stale_line(app, error)), banner);
    }
}

/// The lines of each answer sent from here, newest first: what is known of it, and on a
/// line of its own what the owner should know of a recorded one (not delivered to its pane).
fn answered_here(app: &App, width: u16) -> Vec<Vec<Line<'static>>> {
    use crate::Outcome;
    let t = &app.theme;
    app.outcomes
        .iter()
        .rev()
        .map(|s| {
            let (mark, color, what) = match &s.state {
                Outcome::Sending => ("…", t.sub, "sending".to_string()),
                Outcome::Recorded { answer_id, .. } => {
                    ("✓", t.ok, format!("recorded, event {answer_id}"))
                }
                Outcome::Refused(reason) => ("!", t.check, format!("not recorded: {reason}")),
                Outcome::Unknown { .. } => ("?", t.check, "outcome unknown; checking".into()),
                Outcome::Open => ("?", t.check, "still open: a, then y resends".into()),
                Outcome::Gone => (
                    "·",
                    t.sub,
                    "no longer open (answered or withdrawn elsewhere)".into(),
                ),
            };
            let text = format!("{} {} · {what}", s.ask.ask_id, s.ask.campaign);
            let mut lines = vec![Line::from(vec![
                sp(format!(" {mark} "), color),
                sp(clip(&text, width.saturating_sub(4) as usize), t.text),
            ])];
            if let Outcome::Recorded { note, .. } = &s.state
                && !note.is_empty()
            {
                lines.push(Line::from(sp(
                    format!("   {}", clip(note, width.saturating_sub(4) as usize)),
                    t.check,
                )));
            }
            lines
        })
        .collect()
}

/// The selected ask in full on the Asks screen, with its own scroll (`ctrl-d ctrl-u`).
fn ask_pane(f: &mut Frame, app: &App, area: Rect) {
    let t = &app.theme;
    let (width, h) = (
        area.width.saturating_sub(4) as usize,
        area.height.saturating_sub(2) as usize,
    );
    let (title, lines) = detail_lines(app, width).unwrap_or_else(|| {
        (
            "ask".into(),
            vec![Line::from(sp("No ask selected: j k choose one", t.sub))],
        )
    });
    let lines = fold(app, lines, width);
    let total = lines.len();
    {
        let mut seen = app.seen.borrow_mut();
        (seen.page, seen.full) = ((h, total), true);
    }
    let first = app.scroll.min(total.saturating_sub(h));
    let title = if total > h && h > 0 {
        format!("{title} · {}-{}/{total}", first + 1, first + h)
    } else {
        title
    };
    let inner = ui::pane(f, t, area, &title, false);
    f.render_widget(
        Paragraph::new(lines.into_iter().skip(first).take(h).collect::<Vec<_>>()),
        inner,
    );
}

/// Nothing is clipped: a line wider than `width` folds onto the next.
fn fold(app: &App, lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .flat_map(|line| {
            if ui::width(&line.spans) <= width {
                return vec![line];
            }
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            wrap(&text, width)
                .into_iter()
                .map(|l| Line::from(sp(l, app.theme.text)))
                .collect()
        })
        .collect()
}

fn stale_line(app: &App, error: &str) -> Line<'static> {
    let t = &app.theme;
    let retry = format!(" · retry {}s", app.fetch.retry_in_s);
    Line::from(vec![
        bold(" ! ", t.check),
        sp(
            format!("{error} · showing {}", ui::hhmm(&app.data.glance.now)),
            t.check,
        ),
        sp(retry, t.dim),
    ])
}

/// The panel switches: one line when wide, else 1-3, 4-6 and 7-9,0,H on lines of their own
/// (7-9, 0 and H folded into the second while all hidden), each wrapped to the width.
fn toggles(app: &App, width: usize, wide: bool, short: bool) -> Vec<Line<'static>> {
    // Each switch names its own key: H is not an eleventh digit.
    const LABELS: [(&str, &str); 11] = [
        ("1", "asks"),
        ("2", "checks"),
        ("3", "camps"),
        ("4", "PRs"),
        ("5", "Shipped"),
        ("6", "Build"),
        ("7", "Decisions"),
        ("8", "Fleet"),
        ("9", "Log"),
        ("0", "Issues"),
        ("H", "Health"),
    ];
    if short {
        let mut line = String::from("panels");
        let mut out = vec![];
        for ((key, _), on) in LABELS.iter().zip(app.config.panels) {
            let part = format!("{key}{}", if on { "+" } else { "-" });
            if line.chars().count() + 1 + part.chars().count() > width {
                out.push(Line::from(sp(std::mem::take(&mut line), app.theme.sub)));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line += &part;
        }
        out.push(Line::from(sp(line, app.theme.sub)));
        return out;
    }
    let mut parts: Vec<String> = LABELS
        .iter()
        .zip(app.config.panels)
        .map(|((key, name), on)| format!("{key} {name}{}", if on { "+" } else { "-" }))
        .collect();
    // Narrow, 7-9, 0 and H all hidden (the default) cost no extra line: the six-panel
    // layout keeps its rows. The help names them.
    let folded = !wide && !app.config.panels[6..].iter().any(|p| *p);
    if folded {
        parts.truncate(6);
        parts.push("7-9,0,H off".into());
    }
    let groups: Vec<&[String]> = if wide {
        vec![&parts[..]]
    } else if folded {
        vec![&parts[..3], &parts[3..]]
    } else {
        vec![&parts[..3], &parts[3..6], &parts[6..]]
    };
    let mut out = vec![];
    for group in groups {
        let mut line = String::new();
        for part in group {
            if !line.is_empty() && line.chars().count() + 3 + part.chars().count() > width {
                out.push(Line::from(sp(std::mem::take(&mut line), app.theme.sub)));
            }
            if !line.is_empty() {
                line += " · ";
            }
            line += part;
        }
        out.push(Line::from(sp(line, app.theme.sub)));
    }
    out
}

/// Instead of the list: before the first snapshot, without one, or with nothing in it.
fn state(f: &mut Frame, app: &App, area: Rect) -> bool {
    let Some((lines, centred)) = state_lines(app, area.width as usize) else {
        return false;
    };
    if !centred {
        f.render_widget(Paragraph::new(lines), area);
        return true;
    }
    let height = (lines.len() as u16).min(area.height);
    let middle = Rect {
        y: area.y + (area.height - height) / 3,
        height,
        ..area
    };
    f.render_widget(Paragraph::new(lines).centered(), middle);
    true
}

/// The state's lines, and whether they are centred (the loading shape is not).
fn state_lines(app: &App, w: usize) -> Option<(Vec<Line<'static>>, bool)> {
    let (t, g, fetch) = (&app.theme, &app.data.glance, &app.fetch);
    let lines = match (&fetch.error, fetch.loaded) {
        (None, false) => {
            // The shape of what is coming, so the pane does not jump when it arrives.
            let bar =
                |pct: usize| Line::from(sp(format!(" {}", "░".repeat(w * pct / 100)), t.rule));
            let lines = vec![
                bar(30),
                bar(88),
                bar(72),
                Line::raw(""),
                bar(26),
                bar(80),
                bar(62),
                bar(84),
                bar(68),
            ];
            return Some((lines, false));
        }
        (Some(error), false) => {
            let mut lines = vec![
                Line::from(bold("Cannot reach the hub", t.check)),
                Line::raw(""),
            ];
            let why = wrap(error, w.saturating_sub(4));
            lines.extend(why.into_iter().take(3).map(|l| Line::from(sp(l, t.sub))));
            lines.push(Line::from(sp("No ledger snapshot yet.", t.sub)));
            lines.push(Line::raw(""));
            lines.push(Line::from(sp(
                format!("try {} · next in {}s", fetch.tries, fetch.retry_in_s),
                t.dim,
            )));
            lines
        }
        _ if app.screen == Screen::Asks && g.needs_you.is_empty() => {
            if app.filter.is_empty() {
                vec![
                    Line::from(bold("✓", t.ok)),
                    Line::raw(""),
                    Line::from(sp("Nothing needs you", t.text)),
                    Line::from(sp("No open owner ask in this snapshot.", t.sub)),
                ]
            } else {
                vec![
                    Line::from(sp("No ask matches the filter", t.sub)),
                    Line::from(sp("Esc clears the filter", t.dim)),
                ]
            }
        }
        _ if app.screen == Screen::Fleet && g.campaigns.is_empty() => {
            let (first, second) = if app.filter.is_empty() {
                (
                    "No listed campaigns",
                    "Not a fleet-wide idle claim · c all campaigns",
                )
            } else {
                ("No campaign matches the filter", "Esc clears the filter")
            };
            vec![Line::from(sp(first, t.sub)), Line::from(sp(second, t.dim))]
        }
        _ if (!app.config.panels[..3].iter().all(|p| *p) || !app.filter.is_empty())
            && g.needs_you.is_empty()
            && g.attention.is_empty()
            && g.campaigns.is_empty() =>
        {
            // Above panels 7-9 the line names what is missing; their own rows follow.
            let first = if !shown(app).iter().any(ledger_panel) {
                "No visible ledger rows"
            } else if !app.filter.is_empty() {
                "No matching asks, checks or campaigns"
            } else if !app.config.panels[..3].iter().any(|p| *p) {
                "Asks, checks and campaigns hidden"
            } else {
                "No asks, checks or campaigns shown"
            };
            let second = match first {
                "No visible ledger rows" => "1-3 show panels · / filters",
                "No matching asks, checks or campaigns" => "Esc clears the filter",
                _ => "1-3 show them · / filters",
            };
            vec![Line::from(sp(first, t.sub)), Line::from(sp(second, t.dim))]
        }
        _ if g.needs_you.is_empty()
            && g.attention.is_empty()
            && g.campaigns.is_empty()
            && g.quiet.count == 0 =>
        {
            vec![
                Line::from(bold("✓", t.ok)),
                Line::raw(""),
                Line::from(sp("No open campaigns", t.text)),
                Line::from(sp("Nothing recorded is waiting on you.", t.sub)),
                Line::raw(""),
                Line::from(vec![bold("c", t.accent), sp(" all campaigns", t.dim)]),
            ]
        }
        _ => return None,
    };
    Some((lines, true))
}

/// Which card a row is listed under, as its rule names it.
pub(crate) fn card(id: &RowId) -> &'static str {
    match id {
        RowId::Ask(_) => "NEEDS YOU",
        RowId::Check(..) => "TO CHECK",
        RowId::Campaign(_) => "CAMPAIGNS",
        RowId::Pr(..) => "MY PRS",
        RowId::Author(_) => "SHIPPED",
        RowId::Run(..) => "BUILD QUEUE",
        RowId::Decision(..) => "DECISIONS",
        RowId::Fleet(_) => "FLEET",
        RowId::Logged(..) => "DECISION LOG",
        RowId::Assignee(..) => "ISSUES",
        RowId::Health(..) => "HEALTH",
        RowId::Card(slot) => [
            "NEEDS YOU",
            "TO CHECK",
            "MY PRS",
            "CAMPAIGNS",
            "SHIPPED",
            "BUILD QUEUE",
            "DECISIONS",
            "DECISION LOG",
            "FLEET",
            "ISSUES",
            "HEALTH",
        ][*slot],
    }
}

/// The Detail screen: the item `I` was pressed on, in full and at full width, from the
/// reads on hand with their ages; it reads nothing. An item no longer listed is said to be
/// gone: no other row takes its place, and no key acts on one.
pub(crate) fn draw_detail(f: &mut Frame, app: &App) {
    let t = &app.theme;
    let area = f.area();
    let wide = area.width >= 100;
    let stale = app.fetch.error.as_deref().filter(|_| app.fetch.loaded);
    let [head, tabs, banner, body, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(u16::from(stale.is_some())),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);
    ui::header(
        f,
        app,
        head,
        if wide {
            vec![sp("detail", t.dim)]
        } else {
            vec![]
        },
    );
    ui::tabs(f, app, tabs);
    if let Some(error) = stale {
        f.render_widget(Paragraph::new(stale_line(app, error)), banner);
    }
    // The pane's inside, as `ui::pane` will draw it, so the text wraps before the title
    // says how much of it there is.
    let (width, h) = (
        body.width.saturating_sub(4) as usize,
        body.height.saturating_sub(2) as usize,
    );
    let ids = ids(app);
    let (title, lines) = match (&app.target, ids.get(app.row)) {
        (None, _) => (
            "detail".to_string(),
            vec![
                Line::from(sp("Nothing selected yet.", t.text)),
                Line::raw(""),
                Line::from(sp(
                    "Select a row first on Dashboard, Asks or Fleet; I shows it here in full.",
                    t.sub,
                )),
            ],
        ),
        (Some(target), None) => (
            format!("detail · from {}", target.from.name()),
            vec![
                Line::from(bold("No longer listed", t.check)),
                Line::raw(""),
                Line::from(sp(
                    "The item opened here was answered, closed, expired, hidden or filtered \
                     out since. No other row takes its place, and no key here acts on one.",
                    t.sub,
                )),
                Line::from(sp(
                    format!("esc or h returns to {}.", target.from.name()),
                    t.dim,
                )),
            ],
        ),
        (Some(target), Some(id)) => {
            let first = ids
                .iter()
                .position(|r| std::mem::discriminant(r) == std::mem::discriminant(id))
                .unwrap_or(app.row);
            let (title, mut lines) = detail_lines(app, width).unwrap_or_default();
            lines.insert(
                0,
                Line::from(sp(
                    format!(
                        "from {} · {} · row {}",
                        target.from.name(),
                        card(id),
                        app.row - first + 1
                    ),
                    t.dim,
                )),
            );
            (title, lines)
        }
    };
    let lines = fold(app, lines, width);
    let total = lines.len();
    app.seen.borrow_mut().page = (h, total);
    let mut keys = hints(app, wide);
    if total > h {
        keys.insert(0, hint("j k", Some("scroll")));
    }
    ui::footer(f, app, foot, &keys);
    let first = app.scroll.min(total.saturating_sub(h));
    let title = if total > h && h > 0 {
        format!("{title} · {}-{}/{total}", first + 1, first + h)
    } else {
        title
    };
    let inner = ui::pane(f, t, body, &title, true);
    f.render_widget(
        Paragraph::new(lines.into_iter().skip(first).take(h).collect::<Vec<_>>()),
        inner,
    );
}

/// The selected row's item in full: an ask's full text and options, what a "to check" row
/// observed, a campaign's lead, lanes and latest note, each source with its age, and the
/// campaign's activity. Lines may be wider than `width`; the caller folds them.
fn detail_lines(app: &App, width: usize) -> Option<(String, Vec<Line<'static>>)> {
    let (t, g) = (&app.theme, &app.data.glance);
    let at = |host: &str, pane: &str| {
        let name = ui::host_name(g, host);
        if g.local(host) {
            sp(format!("{name} {pane}"), t.sub)
        } else {
            sp(format!("{name} {pane} ↗"), t.remote)
        }
    };
    let (title, mut lines, text) = match selected(app) {
        Selected::Ask(a) => {
            let head = vec![
                if a.blocking {
                    bold("BLOCKING", t.need)
                } else {
                    sp("not blocking", t.sub)
                },
                sp(
                    if a.asker_waiting {
                        " · asker waiting · "
                    } else {
                        " · asker not waiting · "
                    },
                    t.dim,
                ),
                sp(format!("{} ago", age(a.age_ms)), t.sub),
                sp(" · ", t.dim),
                at(&a.host, &a.pane_id),
            ];
            // A structured ask's options are listed under its text, so the text drops them.
            let text = if a.question.is_some() {
                ask::parse(a).context
            } else {
                a.text.clone()
            };
            (
                format!("ask {} · {}", a.ask_id, a.campaign),
                vec![Line::from(head)],
                text,
            )
        }
        Selected::Check(a) => {
            let head = vec![
                bold(a.text.clone(), t.check),
                sp(format!(" · {} ago · ", age(a.age_ms)), t.dim),
                at(&a.host, &a.pane_id),
            ];
            (
                format!("check · {}", check_name(g, a)),
                vec![Line::from(head)],
                String::new(),
            )
        }
        Selected::Campaign(c) => {
            // Every fact a narrow Fleet row drops: the lead's word, the host, counts with
            // `?` for one the snapshot left out (never pips that would read as zero), age.
            let mark = if c.parked {
                sp("‖", t.dim)
            } else {
                ui::lead_mark(t, &c.lead, c.lead_waiting)
            };
            let mut head = vec![
                sp("lead ", t.dim),
                mark,
                sp(
                    format!(
                        " {}   lanes w/r/o {}",
                        crate::fleet::state(c),
                        crate::fleet::counts(c)
                    ),
                    t.sub,
                ),
            ];
            if !g.local(&c.host) {
                head.push(sp(format!(" · {}", ui::host_name(g, &c.host)), t.remote));
            }
            head.push(sp(format!(" · {} ago", age(c.activity_age_ms)), t.sub));
            if let Some(label) = c.tmp_label() {
                head.push(sp(format!(" · {label}"), t.dim));
            }
            (
                format!("{} · #{}", c.name, c.id),
                vec![Line::from(head)],
                c.status_text().to_string(),
            )
        }
        Selected::Pr(p) => {
            let mine = &app.mine;
            let mut text = p.detail(app.observed_at);
            text.push(String::new());
            // The read it came from: its age, or stale or unavailable, and its cost.
            text.push(format!("source: {}", mine.status(app.observed_tick)));
            text.extend(mine.notes().into_iter().map(|(n, _)| n));
            if let Some(r) = mine.sample.rate {
                text.push(format!(
                    "GraphQL last cycle: cost {} · {} remaining · resets in {}",
                    r.cost,
                    r.remaining,
                    age(r.reset.saturating_sub(app.observed_at).saturating_mul(1000))
                ));
            }
            (
                format!("PR #{} · {}", p.number, p.repo),
                vec![Line::from(sp(p.facts(), t.sub))],
                text.join("\n"),
            )
        }
        Selected::Author(author) => {
            let groups = app.shipped.authors(app.observed_at);
            let rows = &groups[&author];
            (
                format!("Shipped · @{author}"),
                vec![Line::from(sp(
                    format!("{} observed / rolling 24h", rows.len()),
                    t.sub,
                ))],
                format!(
                    "Observed cached sample; not a full total.\n{}\n{}",
                    app.shipped
                        .sources
                        .iter()
                        .map(|s| format!(
                            "{}{}",
                            s.status(app.observed_tick),
                            if s.total == 100 {
                                "; first 100; more may exist"
                            } else {
                                ""
                            }
                        ))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    rows.iter()
                        .map(|p| format!("{} #{} {}", p.repo, p.number, p.title))
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
            )
        }
        Selected::Run(r) => (
            format!("{} · run {}", r.repo, r.id),
            vec![Line::from(sp(r.label(), t.sub))],
            format!(
                "{}\nBranch: {}\nLatest100-run sample; older active runs may be missing.\n{}",
                r.workflow,
                r.branch,
                app.builds
                    .sources
                    .iter()
                    .find(|s| s.repo == r.repo)
                    .map_or_else(String::new, |s| s.status(app.observed_tick))
            ),
        ),
        Selected::Decision(e) => (
            format!("decision #{} · {}", e.id, app.data.campaign.root.name),
            vec![Line::from(sp(
                format!(
                    "in force · cached read {} ago",
                    app.scope.age(app.observed_at).unwrap_or_default()
                ),
                t.sub,
            ))],
            format!(
                "{}\nFrom the campaign read on hand. Glance r does not refresh it; o reads it again.",
                e.text
            ),
        ),
        Selected::Logged(e) => (
            format!(
                "logged decision #{} · {}",
                e.id, app.data.campaign.root.name
            ),
            vec![Line::from(sp(
                format!("{} · {} · recent log sample", e.at, e.lane),
                t.sub,
            ))],
            format!(
                "{}\nHistory from the read's recent log sample, not necessarily in force.",
                e.text
            ),
        ),
        Selected::Fleet(c) => {
            let mut head = vec![
                sp("lead ", t.dim),
                ui::lead_mark(t, &c.lead, c.lead_waiting),
                sp(format!(" {}", c.lead), t.sub),
            ];
            if c.parked {
                head.push(sp(" · parked", t.dim));
            }
            let [w, r, o] = c
                .lanes
                .observed()
                .map(|n| n.map_or("?".into(), |n| n.to_string()));
            (
                format!("{} · #{} · fleet", c.name, c.id),
                vec![Line::from(head)],
                format!(
                    "{w} working · {r} ready · {o} open lanes, as the glance reported them; ? \
                     where it did not.\nListed campaigns only: not every host or agent.\n{}",
                    c.status_text()
                ),
            )
        }
        Selected::Assignee(g) => (
            format!("Issues · {}", g.name()),
            vec![Line::from(sp(g.counts(), t.sub))],
            format!(
                "Current assignees on observed, non-atomic samples of {}. Groups overlap: an \
                 issue with several assignees counts in each, so groups do not sum to the \
                 headline. Blocked: open issues labelled blocked or needs-info. Closed: \
                 closedAt in the rolling 24 hours. Not a global total.\n{}",
                g.repo,
                app.issues
                    .sources
                    .iter()
                    .filter(|s| s.repo == g.repo)
                    .map(|s| {
                        let mut line = s.status(app.observed_tick);
                        for (n, which) in [(s.total, "open"), (s.closed_total, "closed")] {
                            if n == 100 {
                                line += &format!("; {which}: first 100, more may exist");
                            }
                        }
                        line
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        ),
        Selected::Health => (
            format!(
                "Health · {} {}",
                app.health.repo.as_deref().unwrap_or_default(),
                crate::health::ENV
            ),
            vec![Line::from(sp(app.health.state(), health_color(app)))],
            app.health.detail(app.observed_at),
        ),
        Selected::Nothing => return None,
    };
    lines.push(Line::raw(""));
    lines.extend(
        wrap(&text, width)
            .into_iter()
            .map(|l| Line::from(sp(l, t.text))),
    );
    let parsed = match selected(app) {
        Selected::Ask(a) => ask::parse(a),
        _ => ask::parse_text(&text),
    };
    if !parsed.options.is_empty() {
        let head = if parsed.multi {
            "Options · pick any"
        } else {
            "Options"
        };
        lines.extend([Line::raw(""), Line::from(bold(head, t.sub))]);
    }
    for o in parsed.options {
        let mut line = vec![bold(format!(" {} ", o.key), t.accent), sp(o.text, t.text)];
        if o.recommended {
            let tag = if parsed.structured {
                "  ★ recommended"
            } else {
                "  recommended"
            };
            line.push(sp(tag, t.ok));
        }
        lines.push(Line::from(line));
        lines.extend(
            wrap(&o.description, width.saturating_sub(4))
                .into_iter()
                .filter(|l| !l.is_empty())
                .map(|l| Line::from(sp(format!("    {l}"), t.dim))),
        );
    }
    let activity = activity(app, width);
    if !activity.is_empty() {
        lines.push(Line::raw(""));
        lines.extend(activity);
    }
    Some((title, lines))
}

/// The selected row's campaign over the last four hours, its lead and lanes, and its
/// latest note.
fn activity(app: &App, width: usize) -> Vec<Line<'static>> {
    let (t, g) = (&app.theme, &app.data.glance);
    let root = match selected(app) {
        Selected::Ask(a) => a.root_id,
        Selected::Check(a) => a.root_id,
        Selected::Campaign(c) | Selected::Fleet(c) => c.id,
        Selected::Decision(_) | Selected::Logged(_) => app.data.campaign.root.id,
        Selected::Pr(_)
        | Selected::Author(_)
        | Selected::Run(_)
        | Selected::Assignee(_)
        | Selected::Health
        | Selected::Nothing => return vec![],
    };
    let Some(c) = g.campaigns.iter().find(|c| c.id == root) else {
        return vec![];
    };
    // Two columns a bar where they fit.
    let bars: Vec<u32> = if c.spark.len() * 2 <= width {
        c.spark.iter().flat_map(|&v| [v, v]).collect()
    } else {
        c.spark.clone()
    };
    let mut lines = vec![Line::from(bold(format!("{} · last 4 h", c.name), t.sub))];
    lines.extend(
        ui::chart(&bars, 3)
            .into_iter()
            .map(|l| Line::from(sp(l, t.accent))),
    );
    // A campaign's or Fleet row's own Detail already says its lead and its note.
    if matches!(selected(app), Selected::Campaign(_) | Selected::Fleet(_)) {
        return lines;
    }
    let mut lead = vec![
        sp("lead ", t.dim),
        ui::lead_mark(t, &c.lead, c.lead_waiting),
        sp(format!(" {}   lanes ", c.lead), t.sub),
    ];
    lead.extend(ui::pips(t, &c.lanes, 7));
    lines.extend([Line::raw(""), Line::from(lead)]);
    let note = wrap(&format!("last {}", c.status_text()), width);
    lines.extend(note.into_iter().map(|l| Line::from(sp(l, t.sub))));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frames::fixture;

    #[test]
    fn tmp_size_yields_to_remote_primary_row_at_46_columns() {
        let mut app = App::new(fixture());
        let c = &mut app.data.glance.campaigns[0];
        c.name = "remote-primary".into();
        c.host = "laptop".into();
        c.lanes = crate::model::LaneCounts {
            working: 3,
            ready: 2,
            open: 7,
            known: [true; 3],
        };
        c.tmp = serde_json::Value::Null;
        let row = |app: &App, width| {
            lines(app, width, 28)
                .iter()
                .map(|l| {
                    l.spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>()
                })
                .find(|r| r.contains("remote-primary"))
                .unwrap()
        };
        let primary = row(&app, 46);
        app.data.glance.campaigns[0].tmp = serde_json::json!("tmp 1.2G");
        let narrow = row(&app, 46);
        assert_eq!(narrow, primary);
        assert!(narrow.contains("●●●●●○○"), "{narrow}");
        assert!(narrow.contains("laptop"), "{narrow}");
        assert!(!narrow.contains("tmp "));
        assert!(row(&app, 80).contains("tmp 1.2G"));
    }

    #[test]
    fn tmp_size_narrow_fixture_keeps_urgent_rows_and_rejects_malformed_values() {
        let mut app = App::new(fixture());
        app.data.glance.campaigns[0].tmp = serde_json::json!("tmp 1.2G");
        let text = |app: &App| -> Vec<String> {
            lines(app, 46, 28)
                .iter()
                .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
                .collect()
        };
        let rows = text(&app);
        assert!(rows.iter().any(|r| r.contains("tmp 1.2G")), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains("BLOCKING")), "{rows:?}");
        assert!(
            rows.iter()
                .all(|r| unicode_width::UnicodeWidthStr::width(r.as_str()) <= 46)
        );
        for value in [
            serde_json::json!(0),
            serde_json::json!("tmp -1"),
            serde_json::json!("tmp 18446744073709551616"),
            serde_json::json!("tmp 1.2.3G"),
            serde_json::json!(null),
        ] {
            app.data.glance.campaigns[0].tmp = value;
            assert!(app.data.glance.campaigns[0].tmp_label().is_none());
            assert!(!text(&app).iter().any(|r| r.contains("tmp ")));
        }
        let bad: Glance =
            serde_json::from_value(serde_json::json!({"campaigns":[{"tmp":{"invalid":true}}]}))
                .unwrap();
        assert!(bad.campaigns[0].tmp_label().is_none());
        app.data.glance.campaigns[0].tmp = serde_json::json!("tmp 0");
        assert!(text(&app).iter().any(|r| r.contains("tmp 0")));
    }

    #[test]
    fn asks_scroll_and_show_their_position() {
        let mut app = App::new(fixture());
        let ask = app.data.glance.needs_you[0].clone();
        app.data.glance.needs_you = vec![ask; 9];
        app.row = 6;
        let text: String = lines(&app, 46, 14)[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("7/9"), "{text}");
    }

    #[test]
    fn issues_stale_compact_cue_and_both_first_100_disclosures_are_drawn() {
        let text = |name: &str| {
            let spec = crate::frames::all()
                .into_iter()
                .find(|s| s.name == name)
                .unwrap();
            crate::frames::text(&crate::frames::render(&spec))
        };
        // Default narrow, 0 on, cursor on an Issues row: the row says stale.
        let stale = text("issues-46x30-stale");
        assert!(stale.contains("▌stale/partial · @alex"), "{stale}");
        for name in ["issues-46x30-cap", "issues-120x40-overflow-detail"] {
            let t = text(name);
            assert!(
                t.contains("open: first 100") && t.contains("closed: first 100"),
                "{name}:\n{t}"
            );
        }
        // The overflow spec's cursor is on an Issues group, not a stale index; Detail shows
        // that group.
        assert!(text("issues-120x40-overflow").contains("▌@alex"));
        assert!(text("issues-120x40-overflow-detail").contains("Issues · @alex"));
    }
}
