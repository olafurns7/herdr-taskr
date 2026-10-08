//! Level 1, the glance: needs you, to check, campaigns. One borderless column under 100
//! columns; two panes (the list and the selected row's detail) from 100.

use std::ops::Range;

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::{
    App, Hit, ask,
    keys::hint,
    model::{Attention, CampaignRow, Glance, Need},
    theme::Theme,
    ui::{self, age, bold, clip, one_line, pad, sp, wrap},
};

/// What fits in the list, decided before anything is drawn.
///
/// Asks and "to check" rows never fold: they keep two lines and scroll. Campaign rows
/// fold progressively: second lines go to the most recently active campaigns until the
/// rows run out, then the parked and quiet rows fold into one line, then campaigns fold
/// into "+N more". So a frame never has more than one blank row while a second line is
/// folded.
pub(crate) struct Plan {
    asks: Range<usize>,
    checks: Range<usize>,
    /// Active campaigns shown, in order.
    shown: usize,
    /// Which of them keep their second line.
    two: Vec<bool>,
    tail: Tail,
}

#[derive(Debug, PartialEq)]
enum Tail {
    None,
    /// One row per parked campaign, then the quiet row.
    Rows,
    /// Parked and quiet campaigns in one line.
    Folded,
    /// Campaigns that do not fit, parked and quiet ones included.
    More(usize),
}

fn section(rows: usize) -> usize {
    if rows == 0 { 0 } else { 1 + 2 * rows }
}

/// The rows of `n` two-line items that fit in `avail` lines under a rule, around `cursor`.
fn window(n: usize, avail: usize, cursor: Option<usize>) -> Range<usize> {
    let fit = (avail.saturating_sub(1) / 2).min(n);
    let start = cursor
        .map_or(0, |i| (i + 1).saturating_sub(fit))
        .min(n - fit);
    start..start + fit
}

pub(crate) fn plan(g: &Glance, height: usize, cursor: usize) -> Plan {
    let (asks, checks) = (g.needs_you.len(), g.attention.len());
    let active: Vec<&CampaignRow> = g.campaigns.iter().filter(|c| !c.parked).collect();
    let parked = g.campaigns.len() - active.len();
    let quiet = usize::from(g.quiet.count > 0);
    // The campaigns keep their rule and one row, whatever the asks need.
    let floor = if g.campaigns.is_empty() && quiet == 0 {
        0
    } else {
        2
    };

    let room = height.saturating_sub(floor + if checks > 0 { 3 } else { 0 });
    let ask_rows = window(asks, room, (cursor < asks).then_some(cursor));
    let room = height.saturating_sub(floor + section(ask_rows.len()));
    let check_rows = window(
        checks,
        room,
        cursor.checked_sub(asks).filter(|&i| i < checks),
    );

    let rows = height.saturating_sub(section(ask_rows.len()) + section(check_rows.len()) + 1);
    let full = active.len() + parked + quiet;
    let (shown, tail, spare) = if floor == 0 {
        (0, Tail::None, 0)
    } else if full <= rows {
        (active.len(), Tail::Rows, rows - full)
    } else if parked + quiet > 0 && active.len() < rows {
        // ponytail: spare rows here go to second lines, not back to parked rows.
        (active.len(), Tail::Folded, rows - active.len() - 1)
    } else {
        let shown = rows.saturating_sub(1).min(active.len());
        (
            shown,
            Tail::More(active.len() - shown + parked + g.quiet.count as usize),
            0,
        )
    };
    let mut by_activity: Vec<usize> = (0..shown).collect();
    by_activity.sort_by_key(|&i| active[i].activity_age_ms);
    let mut two = vec![false; shown];
    for &i in by_activity.iter().take(spare) {
        two[i] = true;
    }
    Plan {
        asks: ask_rows,
        checks: check_rows,
        shown,
        two,
        tail,
    }
}

/// `2`, or `3/9` when the section is longer than its window.
fn count(rows: &Range<usize>, total: usize, cursor: Option<usize>) -> String {
    if rows.len() == total {
        return total.to_string();
    }
    format!("{}/{total}", cursor.map_or(rows.end, |i| i + 1))
}

fn place(t: &Theme, g: &Glance, host: &str) -> Span<'static> {
    if host == g.caller_host {
        sp("⏎ ", t.accent)
    } else {
        sp(format!("{} ↗ ", ui::host_name(g, host)), t.remote)
    }
}

#[cfg(test)]
pub(crate) fn lines(app: &App, width: usize, height: usize) -> Vec<Line<'static>> {
    layout(app, width, height).0
}

/// The list's lines, and for each line the cursor row it belongs to.
fn layout(app: &App, width: usize, height: usize) -> (Vec<Line<'static>>, Vec<Option<usize>>) {
    let (t, g) = (&app.theme, &app.data.glance);
    let mut hits: Vec<Option<usize>> = vec![];
    let plan = plan(g, height, app.row);
    let body = width.saturating_sub(1);
    let second = |text: &str, color| {
        vec![
            Span::raw("  "),
            sp(clip(&one_line(text), body.saturating_sub(3)), color),
        ]
    };
    let mut out = vec![];
    let mut index = 0;

    let asks = g.needs_you.len();
    if asks > 0 {
        let at = (app.row < asks).then_some(app.row);
        out.push(ui::rule(
            t,
            "NEEDS YOU",
            &count(&plan.asks, asks, at),
            t.need,
            width,
        ));
    }
    for (i, a) in g.needs_you.iter().enumerate() {
        hits.resize(out.len(), None);
        if plan.asks.contains(&i) {
            let on = app.row == index;
            let mut left = vec![
                bold("? ", t.need),
                bold(pad(&a.campaign, 20.min(body.saturating_sub(22))), t.text),
            ];
            if a.blocking {
                left.push(bold(" BLOCKING", t.need));
            }
            let right = vec![
                Span::raw(" "),
                place(t, g, &a.host),
                sp(format!("{:>4}", age(a.age_ms)), t.sub),
            ];
            out.push(ui::row(
                t,
                ui::spread(left, right, body),
                on,
                false,
                t.need,
                width,
            ));
            out.push(ui::row(t, second(&a.text, t.sub), on, false, t.need, width));
        }
        hits.resize(out.len(), Some(index));
        index += 1;
    }

    let checks = g.attention.len();
    if checks > 0 {
        let at = app.row.checked_sub(asks).filter(|&i| i < checks);
        out.push(ui::rule(
            t,
            "TO CHECK",
            &count(&plan.checks, checks, at),
            t.check,
            width,
        ));
    }
    for (i, a) in g.attention.iter().enumerate() {
        hits.resize(out.len(), None);
        if plan.checks.contains(&i) {
            let on = app.row == index;
            let left = vec![bold("! ", t.check), sp(a.campaign.clone(), t.text)];
            let host = if a.host == g.caller_host {
                String::new()
            } else {
                ui::host_name(g, &a.host)
            };
            let right = vec![
                sp(format!(" {host}"), t.remote),
                sp(format!("{:>5}", age(a.age_ms)), t.sub),
            ];
            out.push(ui::row(
                t,
                ui::spread(left, right, body),
                on,
                false,
                t.check,
                width,
            ));
            out.push(ui::row(
                t,
                second(&a.text, t.check),
                on,
                false,
                t.check,
                width,
            ));
        }
        hits.resize(out.len(), Some(index));
        index += 1;
    }

    if plan.tail == Tail::None {
        hits.resize(out.len(), None);
        return (out, hits);
    }
    let active: Vec<&CampaignRow> = g.campaigns.iter().filter(|c| !c.parked).collect();
    out.push(ui::rule(
        t,
        "CAMPAIGNS",
        &active.len().to_string(),
        t.sub,
        width,
    ));
    // mark, name, seven pips, the sparkline, then ten columns for the host and the age.
    // Under 40 columns the sparkline goes first (V2).
    let name_w = if body >= 53 {
        20
    } else {
        18.min(body.saturating_sub(21))
    };
    let spark_w = if width < 40 {
        0
    } else {
        body.saturating_sub(name_w + 20).clamp(6, 16)
    };
    for (i, c) in active.iter().enumerate() {
        hits.resize(out.len(), None);
        if i < plan.shown {
            let on = app.row == index;
            let mut left = vec![
                ui::lead_mark(t, &c.lead, c.lead_waiting),
                Span::raw(" "),
                sp(pad(&c.name, name_w), t.text),
            ];
            left.extend(ui::pips(t, &c.lanes, 7));
            let host = if c.host == g.caller_host {
                String::new()
            } else {
                ui::host_name(g, &c.host)
            };
            let right = vec![
                sp(format!(" {host} "), t.remote),
                sp(format!("{:>3}", age(c.activity_age_ms)), t.sub),
            ];
            // A long machine name takes its room from the sparkline.
            let bars = spark_w.saturating_sub(ui::width(&right).saturating_sub(10));
            left.push(sp(format!(" {}", ui::spark(&c.spark, bars)), t.accent));
            let changed = app.changed.contains(&c.id);
            out.push(ui::row(
                t,
                ui::spread(left, right, body),
                on,
                changed,
                t.accent,
                width,
            ));
            if plan.two[i] {
                out.push(ui::row(
                    t,
                    second(c.status_text(), t.dim),
                    on,
                    false,
                    t.accent,
                    width,
                ));
            }
        }
        hits.resize(out.len(), Some(index));
        index += 1;
    }
    let parked: Vec<&CampaignRow> = g.campaigns.iter().filter(|c| c.parked).collect();
    match plan.tail {
        Tail::Rows => {
            for c in &parked {
                hits.resize(out.len(), None);
                let left = vec![sp("‖ ", t.dim), sp(c.name.clone(), t.dim)];
                let right = vec![sp(format!(" parked {:>3}", age(c.park_age_ms)), t.dim)];
                out.push(ui::row(
                    t,
                    ui::spread(left, right, body),
                    app.row == index,
                    false,
                    t.accent,
                    width,
                ));
                hits.resize(out.len(), Some(index));
                index += 1;
            }
            if g.quiet.count > 0 {
                out.push(Line::from(sp(format!(" · {} quiet", g.quiet.count), t.dim)));
            }
        }
        Tail::Folded => {
            let mut parts = vec![];
            if !parked.is_empty() {
                parts.push(format!("{} parked", parked.len()));
            }
            if g.quiet.count > 0 {
                parts.push(format!("{} quiet", g.quiet.count));
            }
            out.push(Line::from(sp(format!(" · {}", parts.join(" · ")), t.dim)));
        }
        Tail::More(n) => out.push(Line::from(vec![
            sp(format!(" +{n} more · "), t.dim),
            bold("c", t.accent),
            sp(" shows all", t.dim),
        ])),
        Tail::None => {}
    }
    hits.resize(out.len(), None);
    (out, hits)
}

/// A row's identity, so the cursor stays on it when a snapshot moves it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RowId {
    Ask(i64),
    Check(i64, String),
    Campaign(i64),
}

/// Every row the cursor can be on, in cursor order.
pub(crate) fn ids(g: &Glance) -> Vec<RowId> {
    let asks = g.needs_you.iter().map(|a| RowId::Ask(a.ask_id));
    let checks = g
        .attention
        .iter()
        .map(|a| RowId::Check(a.root_id, a.kind.clone()));
    let campaigns = g.campaigns.iter().filter(|c| !c.parked);
    let parked = g.campaigns.iter().filter(|c| c.parked);
    asks.chain(checks)
        .chain(campaigns.chain(parked).map(|c| RowId::Campaign(c.id)))
        .collect()
}

/// Where each section starts, for Tab: asks, checks, campaigns.
pub(crate) fn sections(g: &Glance) -> Vec<usize> {
    let (asks, checks) = (g.needs_you.len(), g.attention.len());
    let mut out = vec![];
    if asks > 0 {
        out.push(0);
    }
    if checks > 0 {
        out.push(asks);
    }
    if !g.campaigns.is_empty() {
        out.push(asks + checks);
    }
    out
}

/// The row under the cursor.
pub(crate) enum Selected<'a> {
    Ask(&'a Need),
    Check(&'a Attention),
    Campaign(&'a CampaignRow),
    Nothing,
}

pub(crate) fn selected(app: &App) -> Selected<'_> {
    let g = &app.data.glance;
    let (asks, checks) = (g.needs_you.len(), g.attention.len());
    if let Some(a) = g.needs_you.get(app.row) {
        return Selected::Ask(a);
    }
    if let Some(a) = app.row.checked_sub(asks).and_then(|i| g.attention.get(i)) {
        return Selected::Check(a);
    }
    let mut campaigns = g
        .campaigns
        .iter()
        .filter(|c| !c.parked)
        .chain(g.campaigns.iter().filter(|c| c.parked));
    app.row
        .checked_sub(asks + checks)
        .and_then(|i| campaigns.nth(i))
        .map_or(Selected::Nothing, Selected::Campaign)
}

/// The footer lists only the keys that do something for the selected row.
pub(crate) fn hints(app: &App, wide: bool) -> Vec<(&'static str, &'static str)> {
    let local = |host: &str| host == app.data.glance.caller_host;
    let go = |host: &str, to: &'static str| match (local(host), wide) {
        (false, _) => hint("⏎", Some("how to get there")),
        (true, true) => hint("⏎", Some(to)),
        (true, false) => hint("⏎", None),
    };
    let mut out = match selected(app) {
        Selected::Ask(a) => vec![go(&a.host, "go to lead"), hint("a", None), hint("l", None)],
        Selected::Check(a) => vec![go(&a.host, "go to lead"), hint("l", None)],
        Selected::Campaign(c) => vec![
            go(&c.host, "go to lead"),
            hint("l", None),
            hint("o", None),
            hint("p", None),
        ],
        Selected::Nothing => vec![hint("c", Some("all campaigns"))],
    };
    if app.fetch.error.is_some() {
        out.insert(0, hint("r", Some("retry")));
    }
    if wide {
        out.extend([
            hint("/", None),
            hint("y", None),
            hint("?", None),
            hint("q", None),
        ]);
    } else {
        out.push(hint("?", None));
    }
    out
}

pub(crate) fn draw(f: &mut Frame, app: &App) {
    let (t, g) = (&app.theme, &app.data.glance);
    let area = f.area();
    let wide = area.width >= 100;
    let stale = app.fetch.error.as_deref().filter(|_| app.fetch.loaded);
    let [head, notes, banner, body, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(u16::from(g.owner_notes_pending > 0)),
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
            vec![sp("glance", t.dim)]
        } else {
            vec![]
        },
    );
    // The migration count: dim, never red, gone at zero.
    let pending = format!(" {} notes still carry OWNER items", g.owner_notes_pending);
    f.render_widget(Paragraph::new(Line::from(sp(pending, t.dim))), notes);
    ui::footer(f, app, foot, &hints(app, wide));

    if state(f, app, body) {
        return;
    }
    let mut list = body;
    if wide {
        // No inset here: the list has its own gutter column.
        let block = ui::panel(t, "Glance", true);
        list = block.inner(Rect { width: 60, ..body });
        f.render_widget(block, Rect { width: 60, ..body });
    }
    let (lines, hits) = layout(app, list.width as usize, list.height as usize);
    // Folded campaigns are out of the cursor's reach; `c` lists them.
    let p = plan(g, list.height as usize, app.row);
    let parked = g.campaigns.iter().filter(|c| c.parked).count();
    app.seen.borrow_mut().rows = g.needs_you.len()
        + g.attention.len()
        + p.shown
        + if p.tail == Tail::Rows { parked } else { 0 };
    for (y, row) in hits.iter().enumerate().take(list.height as usize) {
        if let Some(row) = row {
            let line = Rect {
                y: list.y + y as u16,
                height: 1,
                ..list
            };
            app.hit(line, Hit::Row(*row));
        }
    }
    f.render_widget(Paragraph::new(lines), list);
    if let Some(error) = stale {
        // The last frame stays, dimmed, and says so.
        ui::dim(f, body, t.dim);
        let retry = format!(" · retry {}s", app.fetch.retry_in_s);
        let line = vec![
            bold(" ! ", t.check),
            sp(format!("{error} · showing {}", ui::hhmm(&g.now)), t.check),
            sp(retry, t.dim),
        ];
        f.render_widget(Paragraph::new(Line::from(line)), banner);
    }
    if wide {
        let right = Rect {
            x: body.x + 60,
            width: body.width - 60,
            ..body
        };
        let [detail_area, context] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(9)]).areas(right);
        detail(f, app, detail_area);
        activity(f, app, context);
    }
}

/// Instead of the list: before the first snapshot, without one, or with nothing in it.
fn state(f: &mut Frame, app: &App, area: Rect) -> bool {
    let (t, g, fetch) = (&app.theme, &app.data.glance, &app.fetch);
    let lines = match (&fetch.error, fetch.loaded) {
        (None, false) => {
            // The shape of what is coming, so the pane does not jump when it arrives.
            let w = area.width as usize;
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
            f.render_widget(Paragraph::new(lines), area);
            return true;
        }
        (Some(error), false) => {
            let mut lines = vec![
                Line::from(bold("Cannot reach the hub", t.check)),
                Line::raw(""),
            ];
            let why = wrap(error, area.width.saturating_sub(4) as usize);
            lines.extend(why.into_iter().take(3).map(|l| Line::from(sp(l, t.sub))));
            lines.push(Line::from(sp(
                "No snapshot yet, so nothing is shown.",
                t.sub,
            )));
            lines.push(Line::raw(""));
            lines.push(Line::from(sp(
                format!("try {} · next in {}s", fetch.tries, fetch.retry_in_s),
                t.dim,
            )));
            lines
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
        _ => return false,
    };
    let height = (lines.len() as u16).min(area.height);
    let middle = Rect {
        y: area.y + (area.height - height) / 3,
        height,
        ..area
    };
    f.render_widget(Paragraph::new(lines).centered(), middle);
    true
}

/// The right pane follows the cursor: an ask's full text and options, what a "to check"
/// row observed, or a campaign's lead, lanes and latest note.
fn detail(f: &mut Frame, app: &App, area: Rect) {
    let (t, g) = (&app.theme, &app.data.glance);
    let at = |host: &str, pane: &str| {
        let name = ui::host_name(g, host);
        if host == g.caller_host {
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
            (
                format!("ask {} · {}", a.ask_id, a.campaign),
                vec![Line::from(head)],
                a.text.clone(),
            )
        }
        Selected::Check(a) => {
            let head = vec![
                bold(a.text.clone(), t.check),
                sp(format!(" · {} ago · ", age(a.age_ms)), t.dim),
                at(&a.host, &a.pane_id),
            ];
            (
                format!("check · {}", a.campaign),
                vec![Line::from(head)],
                String::new(),
            )
        }
        Selected::Campaign(c) => {
            let mut head = vec![
                sp("lead ", t.dim),
                ui::lead_mark(t, &c.lead, c.lead_waiting),
                sp(format!(" {}   lanes ", c.lead), t.sub),
            ];
            head.extend(ui::pips(t, &c.lanes, 7));
            (
                format!("{} · #{}", c.name, c.id),
                vec![Line::from(head)],
                c.status_text().to_string(),
            )
        }
        Selected::Nothing => return,
    };
    let inner = ui::pane(f, t, area, &title, false);
    lines.push(Line::raw(""));
    lines.extend(
        wrap(&text, inner.width as usize)
            .into_iter()
            .map(|l| Line::from(sp(l, t.text))),
    );
    let options = ask::parse(&text).options;
    if !options.is_empty() {
        lines.extend([Line::raw(""), Line::from(bold("Options", t.sub))]);
    }
    for o in options {
        let mut line = vec![bold(format!(" {} ", o.key), t.accent), sp(o.text, t.text)];
        if o.recommended {
            line.push(sp("  recommended", t.ok));
        }
        lines.push(Line::from(ui::fit(line, inner.width as usize)));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// The lower right pane: the selected row's campaign over the last four hours.
fn activity(f: &mut Frame, app: &App, area: Rect) {
    let (t, g) = (&app.theme, &app.data.glance);
    let root = match selected(app) {
        Selected::Ask(a) => a.root_id,
        Selected::Check(a) => a.root_id,
        Selected::Campaign(c) => c.id,
        Selected::Nothing => return,
    };
    let Some(c) = g.campaigns.iter().find(|c| c.id == root) else {
        return;
    };
    let inner = ui::pane(f, t, area, &format!("{} · last 4 h", c.name), false);
    let wide: Vec<u32> = c.spark.iter().flat_map(|&v| [v, v]).collect();
    let mut lines: Vec<Line> = ui::chart(&wide, 3)
        .into_iter()
        .map(|l| Line::from(sp(l, t.accent)))
        .collect();
    let mut lead = vec![
        sp("lead ", t.dim),
        ui::lead_mark(t, &c.lead, c.lead_waiting),
        sp(format!(" {}   lanes ", c.lead), t.sub),
    ];
    lead.extend(ui::pips(t, &c.lanes, 7));
    lines.extend([Line::raw(""), Line::from(lead)]);
    let note = ui::wrap_max(
        &format!("last {}", c.status_text()),
        inner.width as usize,
        2,
    );
    lines.extend(note.into_iter().map(|l| Line::from(sp(l, t.sub))));
    f.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frames::fixture;

    /// The owner's rule for the split pane: no wasted rows while text is folded.
    #[test]
    fn folding_leaves_at_most_one_blank_row() {
        let mut app = App::new(fixture());
        for cursor in [0, 3, 9] {
            app.row = cursor;
            for height in 12..70 {
                let p = plan(&app.data.glance, height, cursor);
                let drawn = lines(&app, 46, height).len();
                assert!(drawn <= height, "height {height}: {drawn} lines");
                let folded = p.two.contains(&false) || p.tail != Tail::Rows;
                assert!(
                    !folded || height - drawn <= 1,
                    "height {height}: {} blank rows while folded",
                    height - drawn
                );
            }
        }
    }

    #[test]
    fn asks_scroll_and_show_their_position() {
        let mut app = App::new(fixture());
        let ask = app.data.glance.needs_you[0].clone();
        app.data.glance.needs_you = vec![ask; 9];
        app.row = 6;
        let p = plan(&app.data.glance, 14, app.row);
        assert_eq!(p.asks, 3..7);
        let text: String = lines(&app, 46, 14)[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("7/9"), "{text}");
    }
}
