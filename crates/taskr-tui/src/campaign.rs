//! Level 2, one campaign: goal and plan, the lane tree, asks and decisions, docs, PRs and
//! the log. Five panes from 100 columns; under that, one pane at a time behind a tab bar.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::{
    App, Hit,
    keys::hint,
    model::{Campaign, Event, Lane},
    theme::Theme,
    ui::{self, age, bold, clip, hhmm, one_line, pad, sp},
};

/// A pane's content at a given width.
type PaneLines = fn(&App, usize) -> Vec<Line<'static>>;

const PANES: [&str; 5] = ["Lanes", "Asks", "Docs", "PRs", "Log"];
const DOCS: usize = 2;

/// One row of the lane tree; `lane` is `None` for the lead.
struct LaneRow<'a> {
    prefix: String,
    lane: Option<&'a Lane>,
}

/// The lead, then its lanes depth first: open ones before closed ones, newest first.
fn tree(c: &Campaign) -> Vec<LaneRow<'_>> {
    fn walk<'a>(c: &'a Campaign, parent: i64, indent: &str, out: &mut Vec<LaneRow<'a>>) {
        let mut kids: Vec<&Lane> = c.lanes.iter().filter(|l| l.parent_id == parent).collect();
        kids.sort_by_key(|l| (l.status == "closed", -l.id));
        for (i, lane) in kids.iter().enumerate() {
            let last = i + 1 == kids.len();
            out.push(LaneRow {
                prefix: format!("{indent}{}", if last { "╰ " } else { "│ " }),
                lane: Some(lane),
            });
            walk(
                c,
                lane.id,
                &format!("{indent}{}", if last { "  " } else { "│ " }),
                out,
            );
        }
    }
    let mut out = vec![LaneRow {
        prefix: String::new(),
        lane: None,
    }];
    walk(c, c.root.id, "", &mut out);
    out
}

/// `◐` working, `●` ready, `○` open, `✓` closed, `⊘` abandoned, `✗` rejected.
fn lane_mark(t: &Theme, lane: &Lane) -> Span<'static> {
    match (
        lane.status.as_str(),
        lane.state.as_str(),
        lane.outcome.as_str(),
    ) {
        ("closed", _, "abandoned") => sp("⊘", t.dim),
        ("closed", _, "rejected") => sp("✗", t.sub),
        ("closed", ..) => sp("✓", t.done),
        (_, "working", _) => sp("◐", t.work),
        (_, "ready", _) => sp("●", t.done),
        (_, "blocked", _) => sp("×", t.text),
        _ => sp("○", t.dim),
    }
}

/// `claude-opus-5-5` + `high` reads `opus 5.5 high`.
fn model(lane: &Lane) -> String {
    let name = match lane
        .model
        .strip_prefix("claude-")
        .and_then(|m| m.split_once('-'))
    {
        Some((family, version)) => format!("{family} {}", version.replace('-', ".")),
        None => lane.model.clone(),
    };
    format!("{name} {}", lane.effort).trim().to_string()
}

fn role(lane: &Lane) -> &str {
    match lane.role.as_str() {
        "implementer" => "impl",
        "reviewer" => "review",
        "researcher" => "research",
        "sub-orchestrator" => "sub-orch",
        other => other,
    }
}

fn lane_line(app: &App, row: &LaneRow, narrow: bool, width: usize) -> Vec<Span<'static>> {
    let (t, c, g) = (&app.theme, &app.data.campaign, &app.data.glance);
    let cols = if narrow { 22 } else { 45 };
    let name_w = width.saturating_sub(cols);
    let Some(lane) = row.lane else {
        let name = vec![
            ui::lead_mark(t, &c.root.lead, false),
            Span::raw(" "),
            bold(c.root.name.clone(), t.text),
        ];
        let mut out = ui::spread(name, vec![], name_w);
        if narrow {
            out.push(sp(format!(" {} ", pad("lead", 16)), t.sub));
        } else {
            let host = ui::host_name(g, &c.root.host);
            out.push(sp(
                format!(
                    " {} {} {} {} ",
                    pad("lead", 8),
                    pad("—", 16),
                    pad(&host, 6),
                    pad(&c.root.pane_id, 6)
                ),
                t.sub,
            ));
        }
        out.push(sp(format!("{:>4}", age(c.root.age_ms)), t.sub));
        return out;
    };
    let closed = lane.status == "closed";
    let (fg, sub) = if closed {
        (t.dim, t.dim)
    } else {
        (t.text, t.sub)
    };
    let name = vec![
        sp(row.prefix.clone(), t.rule),
        lane_mark(t, lane),
        sp(format!(" {}", lane.name), fg),
    ];
    let mut out = ui::spread(name, vec![], name_w);
    let model = if lane.model.is_empty() {
        "—".to_string()
    } else {
        model(lane)
    };
    if narrow {
        // Mark, name, model or role, age. Host and pane move to the strip under the table.
        let what = if lane.model.is_empty() {
            role(lane).to_string()
        } else {
            model
        };
        out.push(sp(format!(" {} ", pad(&what, 16)), sub));
    } else {
        let host = ui::host_name(g, &lane.host);
        let host = sp(
            format!("{} ", pad(&host, 6)),
            if g.local(&lane.host) { sub } else { t.remote },
        );
        out.push(sp(
            format!(" {} {} ", pad(role(lane), 8), pad(&model, 16)),
            sub,
        ));
        out.extend([host, sp(format!("{} ", pad(&lane.pane_id, 6)), sub)]);
    }
    out.push(sp(format!("{:>4}", age(lane.age_ms)), sub));
    out
}

/// Under the table: the selected lane's last summary and whether its brief and report
/// were captured. This replaces a "last summary" column, which does not fit.
fn strip(app: &App, row: &LaneRow, narrow: bool, width: usize) -> Vec<Line<'static>> {
    let (t, c, g) = (&app.theme, &app.data.campaign, &app.data.glance);
    let kept = |on: bool| {
        if on {
            sp("✓", t.done)
        } else {
            sp("—", t.dim)
        }
    };
    let mut out = vec![Line::from(sp("─".repeat(width), t.rule))];
    let Some(lane) = row.lane else {
        let head = vec![
            ui::lead_mark(t, &c.root.lead, false),
            bold(format!(" {}", c.root.name), t.text),
            sp(format!("  #{}  lead · {}", c.root.id, c.root.lead), t.dim),
        ];
        out.push(Line::from(ui::fit(head, width)));
        out.extend(
            ui::wrap_max(&format!("next {}", c.root.next), width, 2)
                .into_iter()
                .map(|l| Line::from(sp(l, t.sub))),
        );
        return out;
    };
    let status = match (lane.status.as_str(), lane.state.as_str()) {
        ("closed", _) => format!("closed {}", lane.outcome),
        (_, "") => "open".to_string(),
        (_, state) => state.to_string(),
    };
    let mut head = vec![
        lane_mark(t, lane),
        bold(format!(" {}", lane.name), t.text),
        sp(format!("  #{}  {status}", lane.id), t.dim),
    ];
    let dot = if narrow { " · " } else { "  ·  " };
    let mut files = vec![
        sp("brief ", t.dim),
        kept(lane.brief),
        sp(" report ", t.dim),
        kept(lane.report),
    ];
    if lane.report {
        files.extend([sp(dot, t.dim), bold("o", t.accent), sp(" reads", t.dim)]);
    }
    if narrow {
        out.push(Line::from(ui::fit(head, width)));
        let host = sp(
            format!("{} {}", ui::host_name(g, &lane.host), lane.pane_id),
            if g.local(&lane.host) { t.sub } else { t.remote },
        );
        let mut line = vec![host, sp(dot, t.dim)];
        line.extend(files);
        out.push(Line::from(ui::fit(line, width)));
    } else {
        head.push(sp(dot, t.dim));
        head.extend(files);
        out.push(Line::from(ui::fit(head, width)));
    }
    out.extend(
        ui::wrap_max(&one_line(&lane.summary), width, 2)
            .into_iter()
            .map(|l| Line::from(sp(l, t.sub))),
    );
    out
}

/// One event: the time, a mark for its kind, then who and what.
fn event(t: &Theme, e: &Event, width: usize) -> Line<'static> {
    let (mark, color) = match e.kind.as_str() {
        "ask" if e.open && e.owner => ("?", t.need),
        "ask" => ("?", t.sub),
        "answer" => ("↳", t.sub),
        "decision" => ("◆", t.accent),
        "done" => ("✓", t.done),
        "ready" => ("●", t.done),
        "fail" => ("✗", t.text),
        "launch" => ("◐", t.work),
        "note" if e.owner => ("•", t.accent),
        _ => ("·", t.dim),
    };
    let text = if e.text.is_empty() {
        e.kind.clone()
    } else {
        one_line(&e.text)
    };
    let fg = if e.kind == "ask" && e.open && e.owner {
        t.need
    } else {
        t.sub
    };
    Line::from(vec![
        sp(format!("{} ", hhmm(&e.at)), t.dim),
        sp(format!("{mark} "), color),
        sp(
            clip(&format!("{}: {text}", e.lane), width.saturating_sub(8)),
            fg,
        ),
    ])
}

/// Open asks first (red), then decisions in force, then recent ask and answer pairs.
fn asks(app: &App, width: usize) -> Vec<Line<'static>> {
    let (t, c) = (&app.theme, &app.data.campaign);
    let open = c.asks.iter().filter(|a| a.open).count();
    let count = if open > 0 {
        bold(format!("{open} open"), t.need)
    } else {
        sp("0 open", t.dim)
    };
    let mut out = vec![Line::from(vec![
        count,
        sp(
            format!("  ·  decisions in force: {}", c.decisions.len()),
            t.dim,
        ),
    ])];
    let rest = c.asks.iter().filter(|a| !a.open);
    out.extend(
        c.asks
            .iter()
            .filter(|a| a.open)
            .chain(&c.decisions)
            .chain(rest)
            .map(|e| event(t, e, width)),
    );
    out
}

fn docs(app: &App, width: usize) -> Vec<Line<'static>> {
    let t = &app.theme;
    let rows = app.data.campaign.docs.iter().enumerate().map(|(i, d)| {
        let name = if d.name.is_empty() { &d.lane } else { &d.name };
        let (kind, fg) = if d.captured {
            (t.accent, t.text)
        } else {
            (t.dim, t.dim)
        };
        let line = Line::from(ui::spread(
            vec![
                sp(pad(&d.kind, 7), kind),
                sp(pad(name, width.saturating_sub(20)), fg),
                sp(format!(" v{:<3}", d.version), t.dim),
                sp(if d.captured { "" } else { "not kept" }, t.dim),
            ],
            vec![],
            width,
        ));
        // Focused, the pane has a cursor: Enter or `o` reads that document.
        if app.pane == DOCS && i == app.scroll {
            line.style(t.selected())
        } else {
            line
        }
    });
    rows.collect()
}

/// The document `o` reads: the one under the Docs cursor, or the selected lane's latest
/// report (the plan, then the goal, for the lead).
pub(crate) fn doc(app: &App) -> Option<&crate::model::DocRow> {
    let c = &app.data.campaign;
    if app.pane == DOCS {
        return c.docs.get(app.scroll);
    }
    let rows = tree(c);
    let latest = |kind: &str, lane: &str| {
        let of = c
            .docs
            .iter()
            .filter(|d| d.captured && d.kind == kind && (lane.is_empty() || d.lane == lane));
        of.max_by_key(|d| d.version)
    };
    match rows.get(app.lane)?.lane {
        Some(lane) => latest("report", &lane.name),
        None => latest("plan", "").or_else(|| latest("goal", "")),
    }
}

/// The first row of a window `height` tall over `total` rows: the cursor's window in
/// Docs, the scroll position elsewhere.
fn first_row(app: &App, pane: usize, height: usize, total: usize) -> usize {
    if app.pane != pane {
        return 0;
    }
    // In Docs `scroll` is the cursor, so it travels to the last row, one at a time.
    app.seen.borrow_mut().page = if pane == DOCS {
        (1, total)
    } else {
        (height, total)
    };
    if pane == DOCS {
        top(app.scroll.min(total.saturating_sub(1)), height)
    } else {
        app.scroll.min(total.saturating_sub(height))
    }
}

fn prs(app: &App, width: usize) -> Vec<Line<'static>> {
    let t = &app.theme;
    let rows = app.data.campaign.prs.iter().map(|p| {
        let ci = match p.ci.as_str() {
            "pass" => sp("✓", t.done),
            "fail" => sp("✗", t.text),
            "running" => sp("◐", t.work),
            _ => sp("·", t.dim),
        };
        // No stored state is unknown, not closed.
        let open = p.state.is_empty() || p.state == "open";
        // The ref as written when it is not a number.
        let number = if p.number > 0 {
            format!("#{}", p.number)
        } else {
            p.value.clone()
        };
        let mut line = vec![];
        // A `pr.<slice>` row leads with its slice: `backend #123`.
        if let Some(slice) = p.key.strip_prefix("pr.") {
            line.push(sp(format!("{slice} "), t.sub));
        }
        line.push(bold(
            format!("{number:<4} "),
            if open { t.accent } else { t.dim },
        ));
        // Title, state and CI are stored for the bare `pr` only; other rows say whose it is.
        if p.title.is_empty() && p.state.is_empty() && p.ci.is_empty() {
            line.push(sp(p.lane.clone(), t.dim));
        } else {
            line.extend([
                ci,
                sp(
                    format!(" {} ", pad(&p.state, 6)),
                    if open { t.text } else { t.dim },
                ),
                sp(p.title.clone(), if open { t.text } else { t.sub }),
            ]);
        }
        Line::from(ui::fit(line, width))
    });
    rows.collect()
}

/// The goal in the header: its first two lines with text, the first without the `#` of a
/// Markdown title.
fn goal(c: &Campaign) -> (&str, &str) {
    let mut lines = c.goal.iter().map(|l| l.trim()).filter(|l| !l.is_empty());
    let title = lines
        .next()
        .unwrap_or("")
        .trim_start_matches('#')
        .trim_start();
    (title, lines.next().unwrap_or(""))
}

fn log(app: &App, width: usize) -> Vec<Line<'static>> {
    app.data
        .campaign
        .log
        .iter()
        .map(|e| event(&app.theme, e, width))
        .collect()
}

/// The first visible row that keeps `cursor` in a view `height` rows tall.
fn top(cursor: usize, height: usize) -> usize {
    (cursor + 1).saturating_sub(height)
}

fn lanes(f: &mut Frame, app: &App, area: Rect, narrow: bool) -> (usize, usize, usize) {
    let (t, c) = (&app.theme, &app.data.campaign);
    let rows = tree(c);
    let strip_h = if narrow { 5 } else { 4 };
    let [head, grid, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(strip_h),
    ])
    .areas(area);
    let width = area.width as usize;
    let titles = if narrow {
        format!(
            " {} {} {:>4}",
            pad("  lane", width.saturating_sub(23)),
            pad("model", 16),
            "age"
        )
    } else {
        let lane = pad("  lane", width.saturating_sub(46));
        format!(
            " {lane} {} {} {} {} {:>4}",
            pad("role", 8),
            pad("model", 16),
            pad("host", 6),
            pad("pane", 6),
            "age"
        )
    };
    f.render_widget(Paragraph::new(Line::from(sp(titles, t.dim))), head);
    let first = top(app.lane, grid.height as usize);
    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .skip(first)
        .take(grid.height as usize)
        .map(|(i, r)| {
            ui::row(
                t,
                lane_line(app, r, narrow, width - 1),
                i == app.lane,
                false,
                t.accent,
                width,
            )
        })
        .collect();
    let shown = lines.len();
    app.seen.borrow_mut().rows = rows.len();
    for y in 0..shown {
        let line = Rect {
            y: grid.y + y as u16,
            height: 1,
            ..grid
        };
        app.hit(line, Hit::Lane(first + y));
    }
    f.render_widget(Paragraph::new(lines), grid);
    if let Some(row) = rows.get(app.lane) {
        let inset = Rect {
            x: foot.x + 1,
            width: foot.width.saturating_sub(2),
            ..foot
        };
        f.render_widget(
            Paragraph::new(strip(app, row, narrow, inset.width as usize)),
            inset,
        );
    }
    if !narrow {
        ui::scrollbar(
            f,
            t,
            Rect {
                x: grid.right(),
                width: 1,
                ..grid
            },
            rows.len(),
            first,
        );
    }
    (first, shown, rows.len())
}

/// The selected lane's machine and pane (the lead's for the first row), and whether it
/// has a report to read.
pub(crate) fn target(app: &App) -> Option<(String, String, bool)> {
    let c = &app.data.campaign;
    let rows = tree(c);
    Some(match rows.get(app.lane)?.lane {
        Some(l) => (l.host.clone(), l.pane_id.clone(), l.report),
        None => (c.root.host.clone(), c.root.pane_id.clone(), false),
    })
}

fn hints(narrow: bool) -> Vec<(&'static str, &'static str)> {
    if narrow {
        return vec![
            hint("⏎", None),
            hint("o", None),
            hint("tab", None),
            hint("h", None),
            hint("?", None),
        ];
    }
    vec![
        hint("⏎", Some("go to pane")),
        hint("o", None),
        hint("tab", Some("next pane")),
        hint("1-5", None),
        hint("/", None),
        hint("h", None),
        hint("?", None),
    ]
}

fn drift(c: &Campaign) -> (String, bool) {
    let (d, l) = (c.plan.decisions_since, c.plan.closed_since);
    let s = |n: u32| if n == 1 { "" } else { "s" };
    (
        format!(
            "{d} decision{} and {l} lane close{} since this version",
            s(d),
            s(l)
        ),
        d + l > 0,
    )
}

pub(crate) fn draw(f: &mut Frame, app: &App) {
    if f.area().width < 100 {
        narrow(f, app)
    } else {
        wide(f, app)
    }
}

fn wide(f: &mut Frame, app: &App) {
    let (t, c) = (&app.theme, &app.data.campaign);
    let [head, top, body, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(6),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(f.area());
    let crumb = vec![
        bold(c.root.name.clone(), t.text),
        sp(
            format!(
                " #{}  {} · started {}",
                c.root.id,
                c.root.status,
                hhmm(&c.root.created_at)
            ),
            t.dim,
        ),
    ];
    ui::header(f, app, head, crumb);
    ui::footer(f, app, foot, &hints(false));

    let inner = ui::pane(f, t, top, "Goal and plan", false);
    let [text, chart] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(26)]).areas(inner);
    let w = text.width as usize;
    let (since, drifted) = drift(c);
    let (title, about) = goal(c);
    let lines = vec![
        Line::from(bold(clip(title, w), t.text)),
        Line::from(sp(clip(about, w), t.sub)),
        Line::from(vec![
            sp("plan ", t.dim),
            sp(format!("v{}", c.plan.version), t.text),
            sp("  ·  ", t.dim),
            sp(since, if drifted { t.check } else { t.dim }),
        ]),
        Line::from(ui::fit(
            vec![sp("next ", t.dim), sp(c.root.next.clone(), t.text)],
            w,
        )),
    ];
    f.render_widget(Paragraph::new(lines), text);
    let mut bars = vec![Line::from(sp("activity · 4 h", t.dim)).right_aligned()];
    bars.extend(
        ui::chart(&c.spark, 3)
            .into_iter()
            .map(|l| Line::from(sp(l, t.accent)).right_aligned()),
    );
    f.render_widget(Paragraph::new(bars), chart);

    let [left, right] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(46)]).areas(body);
    let open = c.lanes.iter().filter(|l| l.status != "closed").count();
    let block = ui::panel(
        t,
        &format!("[1] Lanes · {open} open · {} closed", c.lanes.len() - open),
        app.pane == 0,
    );
    let inner = block.inner(left);
    f.render_widget(block, left);
    app.hit(left, Hit::Pane(0));
    lanes(f, app, inner, false);

    let [a, d, p, l] = Layout::vertical([
        Constraint::Length(9),
        Constraint::Length(7),
        Constraint::Length(5),
        Constraint::Min(0),
    ])
    .areas(right);
    let panes: [(Rect, &str, PaneLines); 4] = [
        (a, "[2] Asks and decisions", asks),
        (d, "[3] Docs", docs),
        (p, "[4] PRs", prs),
        (l, "[5] Log", log),
    ];
    for (i, (area, title, lines)) in panes.into_iter().enumerate() {
        // A pane longer than its box scrolls when focused and says where it is.
        let lines = lines(app, area.width.saturating_sub(4) as usize);
        let (total, h) = (lines.len(), area.height.saturating_sub(2) as usize);
        let first = first_row(app, i + 1, h, total);
        app.hit(area, Hit::Pane(i + 1));
        let title = if total > h {
            format!("{title} · {}-{}/{total}", first + 1, first + h)
        } else {
            title.to_string()
        };
        let inner = ui::pane(f, t, area, &title, app.pane == i + 1);
        f.render_widget(
            Paragraph::new(lines.into_iter().skip(first).take(h).collect::<Vec<_>>()),
            inner,
        );
    }
}

fn narrow(f: &mut Frame, app: &App) {
    let (t, c) = (&app.theme, &app.data.campaign);
    let area = f.area();
    let w = area.width as usize;
    let [head, meta, tabs, body, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(2),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);
    ui::header(f, app, head, vec![bold(c.root.name.clone(), t.text)]);
    ui::footer(f, app, foot, &hints(true));

    let (since, drifted) = drift(c);
    let facts = vec![sp(
        format!(
            " #{} · {} · started {}",
            c.root.id,
            c.root.status,
            hhmm(&c.root.created_at)
        ),
        t.dim,
    )];
    let bars = vec![sp(format!("{} ", ui::spark(&c.spark, 12)), t.accent)];
    let since = since
        .replace(" since this version", " since")
        .replace(" and ", ", ");
    let lines = vec![
        Line::from(ui::spread(facts, bars, w)),
        Line::from(ui::fit(
            vec![
                sp(format!(" plan v{} · ", c.plan.version), t.dim),
                sp(since, if drifted { t.check } else { t.dim }),
            ],
            w,
        )),
        Line::from(ui::fit(
            vec![sp(" next ", t.dim), sp(c.root.next.clone(), t.text)],
            w - 1,
        )),
    ];
    f.render_widget(Paragraph::new(lines), meta);

    let (first, shown, total) = match app.pane {
        0 => lanes(f, app, body, true),
        n => {
            let lines = [asks, docs, prs, log][n.min(4) - 1](app, w - 2);
            let (total, h) = (lines.len(), body.height as usize);
            let first = first_row(app, n, h, total);
            let inset = Rect {
                x: body.x + 1,
                width: body.width - 2,
                ..body
            };
            f.render_widget(
                Paragraph::new(lines.into_iter().skip(first).take(h).collect::<Vec<_>>()),
                inset,
            );
            (first, total.min(h), total)
        }
    };

    // The tab bar, and under it a rule that is accent below the open tab and ends in the
    // position when the pane scrolls. Only what is open is counted.
    let counts = [
        c.lanes.iter().filter(|l| l.status != "closed").count(),
        c.asks.iter().filter(|a| a.open).count(),
        0,
        0,
        0,
    ];
    let mut bar = vec![Span::raw(" ")];
    let mut under = vec![sp("─", t.rule)];
    for (i, name) in PANES.iter().enumerate() {
        let label = if counts[i] > 0 {
            format!("{name} {}", counts[i])
        } else {
            (*name).to_string()
        };
        let on = i == app.pane;
        let tab = Rect {
            x: tabs.x + ui::width(&bar) as u16,
            width: ui::width(&[Span::raw(label.clone())]) as u16,
            height: 1,
            ..tabs
        };
        app.hit(tab, Hit::Pane(i));
        under.push(sp(
            "─".repeat(ui::width(&[Span::raw(label.clone())])),
            if on { t.accent } else { t.rule },
        ));
        bar.push(if on {
            bold(label, t.accent)
        } else {
            sp(label, t.sub)
        });
        if i + 1 < PANES.len() {
            bar.push(sp(" · ", t.rule));
            under.push(sp("───", t.rule));
        }
    }
    let position = if shown < total {
        format!(" {}-{}/{total} ", first + 1, first + shown)
    } else {
        String::new()
    };
    let room = w.saturating_sub(position.len());
    under.push(sp(
        "─".repeat(room.saturating_sub(ui::width(&under))),
        t.rule,
    ));
    let mut under = ui::fit(under, room);
    under.push(sp(position, t.dim));
    f.render_widget(
        Paragraph::new(vec![Line::from(bar), Line::from(under)]),
        tabs,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lanes_form_a_tree_with_open_ones_first() {
        let lane = |id, parent_id, status: &str| Lane {
            id,
            parent_id,
            status: status.into(),
            ..Lane::default()
        };
        let c = Campaign {
            root: crate::model::Root {
                id: 1,
                ..Default::default()
            },
            lanes: vec![
                lane(2, 1, "closed"),
                lane(3, 1, "open"),
                lane(4, 3, "open"),
                lane(5, 3, "closed"),
            ],
            ..Campaign::default()
        };
        let rows: Vec<(String, i64)> = tree(&c)
            .iter()
            .map(|r| (r.prefix.clone(), r.lane.map_or(1, |l| l.id)))
            .collect();
        let want = [("", 1), ("│ ", 3), ("│ │ ", 4), ("│ ╰ ", 5), ("╰ ", 2)];
        assert_eq!(rows, want.map(|(p, id)| (p.to_string(), id)));
        let named = Lane {
            model: "claude-opus-5-5".into(),
            effort: "high".into(),
            ..Lane::default()
        };
        assert_eq!(model(&named), "opus 5.5 high");
    }

    #[test]
    fn the_goal_header_and_the_pr_rows() {
        let with = |goal: &[&str]| Campaign {
            goal: goal.iter().map(|l| l.to_string()).collect(),
            ..Campaign::default()
        };
        let c = with(&["", "#  Demo checkout", "  ", "Ship the sample.", "More."]);
        assert_eq!(goal(&c), ("Demo checkout", "Ship the sample."));
        assert_eq!(goal(&with(&["Only a title"])), ("Only a title", ""));
        assert_eq!(goal(&Campaign::default()), ("", ""));

        // One line per PR-valued ref; a slice row names its slice and its lane.
        let mut app = App::new(crate::frames::fixture());
        let text = |app: &App| -> Vec<String> {
            let lines = prs(app, 44);
            lines
                .iter()
                .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
                .collect()
        };
        let rows = text(&app);
        assert_eq!(rows.len(), 3);
        assert!(
            rows[0].starts_with("#215 ◐ open   feat(glance)"),
            "{}",
            rows[0]
        );
        assert_eq!(rows[1], "backend #214 impl-list-polish");
        assert_eq!(rows[2], "docs #213 impl-list-polish");
        // A ref that is not a number shows as written.
        app.data.campaign.prs[1].number = 0;
        app.data.campaign.prs[1].value = "demo-org/demo#7".into();
        assert_eq!(text(&app)[1], "backend demo-org/demo#7 impl-list-polish");
    }
}
