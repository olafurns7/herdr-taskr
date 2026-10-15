//! The Fleet screen (proposal §8): every listed campaign in one full-width table, parked
//! ones last and quiet ones folded into a line, the totals of what was listed, then the
//! lanes of the campaign last read on purpose (`l`), as cached. Moving the cursor reads
//! nothing, and the lanes never follow it.

use ratatui::{
    Frame,
    layout::{Constraint, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Cell, Paragraph, Row, Table},
};
use unicode_width::UnicodeWidthStr;

use crate::{
    App, Hit, campaign,
    glance::{self, RowId},
    model::{Campaign, CampaignRow},
    ui::{self, age, bold, clip, one_line, sp},
};

/// The roster's column widths; 0 leaves a column out. The lead mark (2 with the cursor
/// gutter) and the age (4) are always drawn.
struct Fit {
    state: usize,
    name: usize,
    host: usize,
    lanes: usize,
    activity: usize,
    milestone: usize,
}

impl Fit {
    /// The columns drawn and the one-cell gaps between them, without the milestone.
    fn used(&self) -> usize {
        let cols = [
            self.state,
            self.name,
            self.host,
            self.lanes,
            self.activity,
            4,
        ];
        let shown = cols.iter().filter(|w| **w > 0);
        shown.clone().sum::<usize>() + shown.count() - 1
    }

    /// The secondary columns give way, least needed first (the four-hour activity, the
    /// host, the lead's word), while the milestone has under 20 columns; under 10 it goes
    /// too. Detail shows every field of the row.
    fn new(width: usize, longest: usize, remote: bool) -> Self {
        let mut f = Fit {
            state: 12,
            name: longest.clamp(8, 20),
            host: if remote { 7 } else { 0 },
            lanes: if width >= 80 { 11 } else { 8 },
            activity: 24,
            milestone: 0,
        };
        for step in 0..3 {
            if width.saturating_sub(f.used() + 1) >= 20 {
                break;
            }
            match step {
                0 => f.activity = 0,
                1 => f.host = 0,
                _ => f.state = 2,
            }
        }
        let room = width.saturating_sub(f.used() + 1);
        if room >= 10 {
            f.milestone = room;
        } else if f.used() > width {
            f.name = f.name.saturating_sub(f.used() - width).max(4);
        }
        f
    }
}

/// The lead column's word: the lead's state, `waiting` while it waits on its lanes,
/// `parked` for a parked campaign, `?` when the snapshot did not say.
pub(crate) fn state(c: &CampaignRow) -> &str {
    match (c.parked, c.lead.as_str(), c.lead_waiting) {
        (true, ..) => "parked",
        (_, "working", _) => "working",
        (_, lead @ ("gone" | "blocked"), _) => lead,
        (_, _, true) => "waiting",
        (_, "", _) => "?",
        (_, lead, _) => lead,
    }
}

/// Working, ready and open lanes, `?` where the snapshot did not say: never a zero.
pub(crate) fn counts(c: &CampaignRow) -> String {
    c.lanes
        .observed()
        .map(|n| n.map_or("?".to_string(), |n| n.to_string()))
        .join("/")
}

/// The totals of the listed campaigns (filtered: of those that match), never of every host;
/// `+?` where a campaign did not report a count. `at` is the roster's window when cut.
fn totals(app: &App, width: usize, at: &str) -> String {
    let rows = &app.data.glance.campaigns;
    let sum = |i: usize| {
        let seen = rows.iter().map(|c| c.lanes.observed()[i]);
        let n: u32 = seen.clone().flatten().sum();
        if seen.clone().any(|n| n.is_none()) {
            format!("{n}+?")
        } else {
            n.to_string()
        }
    };
    let (w, r, o) = (sum(0), sum(1), sum(2));
    let unknown = rows
        .iter()
        .any(|c| c.lanes.observed().iter().any(Option::is_none));
    let n = rows.len();
    let listed = app.listed.1;
    let scope = match (app.filter.is_empty(), n) {
        (true, 1) => "1 listed campaign".to_string(),
        (true, _) => format!("{n} listed campaigns"),
        (false, _) => format!(
            "{n} of {listed} listed campaigns matching \"{}\"",
            app.filter
        ),
    };
    let missing = if unknown { " · ? not reported" } else { "" };
    let wide = format!(
        " {at}totals  working {w} · ready {r} · open {o} lanes in {scope} \
         (listed only, not every host){missing}"
    );
    if wide.width() <= width {
        return wide;
    }
    let short = if app.filter.is_empty() {
        format!("{n} listed only")
    } else {
        format!("{n} of {listed} listed")
    };
    let missing = if unknown { " · ? unreported" } else { "" };
    clip(
        &format!(" {at}{short} · {w}/{r}/{o} lanes w/r/o{missing}"),
        width,
    )
}

/// `before`, `name`, then `required`, in `width`: the optional parts give way from the last,
/// then the name is clipped, so `required` (the root and the read age) stays whole.
fn fitted(width: usize, before: &str, name: &str, required: &str, optional: &[&str]) -> String {
    let mut extra = optional.to_vec();
    loop {
        let tail = format!("{required}{}", extra.concat());
        let room = width.saturating_sub(before.width() + tail.width());
        if room >= name.width().min(8) || extra.is_empty() {
            return format!("{before}{}{tail}", clip(name, room.max(4)));
        }
        extra.pop();
    }
}

/// The cached lanes under the roster, in at most `height` lines: the campaign last read on
/// purpose, by its own root, name and read age, never the cursor's campaign. Under three
/// lines, one line says what is cached.
fn preview(app: &App, width: usize, height: usize) -> Vec<Line<'static>> {
    let (t, g) = (&app.theme, &app.data.glance);
    let read = glance::scoped(app);
    let age = app.scope.age(app.observed_at).unwrap_or_default();
    let error = app.scope.error.as_deref();
    // A deliberate read still on its way; the lanes on hand stay until it lands.
    let reading = app.scope.want.map(|(root, ..)| {
        g.campaigns
            .iter()
            .find(|c| c.id == root)
            .map_or(format!("#{root}"), |c| c.name.clone())
    });
    // `l` acts on the cursor's row: it re-reads the cached campaign only from that row.
    let same = read.is_some_and(
        |c| matches!(glance::selected(app), glance::Selected::Campaign(s) if s.id == c.root.id),
    );
    if height < 3 {
        let (text, color) = match (read, error, &reading) {
            (Some(c), Some(_), _) => (
                fitted(
                    width,
                    " ! LANES ",
                    &c.root.name,
                    &format!(" #{} · last good read {age} ago", c.root.id),
                    &[" · a later read failed"],
                ),
                t.check,
            ),
            (Some(c), None, _) => {
                let open = format!(
                    " · {} open",
                    c.lanes.iter().filter(|l| l.status != "closed").count()
                );
                let hint = if same { " · l re-reads" } else { "" };
                (
                    fitted(
                        width,
                        " LANES ",
                        &c.root.name,
                        &format!(" #{} · read {age} ago", c.root.id),
                        &[&open, hint],
                    ),
                    t.sub,
                )
            }
            (None, Some(error), _) => (format!(" ! {error}"), t.check),
            (None, None, Some(name)) => (format!(" reading {name}…"), t.sub),
            (None, None, None) => (
                " l opens a campaign and reads its lanes; the cursor never reads".into(),
                t.dim,
            ),
        };
        let line = Line::from(sp(clip(&text, width), color));
        return std::iter::once(line).take(height).collect();
    }
    let mut out = vec![match read {
        Some(c) => {
            // The root and the read age stay whole; the hint, then the name, give way.
            let tag = format!(" #{}", c.root.id);
            let room = |right: &str| {
                width.saturating_sub("LANES · ".width() + tag.width() + right.width() + 5)
            };
            let mut right = format!("read {age} ago");
            if same && room(&format!("{right} · l re-reads")) >= c.root.name.width().min(8) {
                right.push_str(" · l re-reads");
            }
            let name = clip(&c.root.name, room(&right).max(4));
            ui::rule(t, &format!("LANES · {name}{tag}"), &right, t.sub, width)
        }
        None => ui::rule(t, "LANES", "nothing read yet", t.sub, width),
    }];
    if let Some(name) = &reading {
        out.push(Line::from(sp(
            clip(&format!(" reading {name}…"), width),
            t.sub,
        )));
    }
    if let Some(error) = error {
        let kept = if read.is_some() {
            " · showing the last good read"
        } else {
            ""
        };
        out.push(Line::from(sp(
            clip(&format!(" ! {error}{kept}"), width),
            t.check,
        )));
    }
    let Some(c) = read else {
        out.push(Line::from(vec![
            Span::raw(" "),
            bold("l", t.accent),
            sp(
                clip(
                    " opens the selected campaign and reads its lanes; they stay here \
                     after it closes. Moving the cursor never reads.",
                    width.saturating_sub(2),
                ),
                t.sub,
            ),
        ]));
        out.truncate(height);
        return out;
    };
    // The lead and its open lanes, as the campaign view's tree draws them.
    let mut open = Campaign {
        root: c.root.clone(),
        lanes: c
            .lanes
            .iter()
            .filter(|l| l.status != "closed")
            .cloned()
            .collect(),
        ..Campaign::default()
    };
    // An open lane under a closed parent hangs from the lead, as the filtered campaign
    // view does, rather than dropping out of the tree.
    let ids: std::collections::HashSet<i64> = open.lanes.iter().map(|l| l.id).collect();
    for lane in &mut open.lanes {
        if !ids.contains(&lane.parent_id) {
            lane.parent_id = c.root.id;
        }
    }
    let narrow = width < 80;
    let lanes: Vec<Line<'static>> = campaign::tree(&open)
        .iter()
        .map(|row| {
            let mut spans = vec![Span::raw(" ")];
            if narrow {
                spans.extend(campaign::lane_line(app, row, true, width.saturating_sub(1)));
            } else {
                spans.extend(campaign::lane_line(
                    app,
                    row,
                    false,
                    width.saturating_sub(8),
                ));
                let report = row.lane.is_some_and(|l| l.report);
                spans.push(sp(if report { " report" } else { "" }, t.done));
            }
            Line::from(spans)
        })
        .collect();
    let room = height.saturating_sub(out.len());
    let shown = if lanes.len() <= room {
        lanes.len()
    } else {
        room.saturating_sub(1)
    };
    out.extend(lanes.into_iter().take(shown));
    // The lead is the first row; closed lanes, and any cut, are counted, never dropped silently.
    let drawn = shown.saturating_sub(1);
    let total = c.lanes.len();
    if drawn < total && out.len() < height {
        out.push(Line::from(sp(
            clip(
                &format!(
                    " {drawn} of {total} lanes, open shown{}",
                    if same { " · l opens all" } else { "" }
                ),
                width,
            ),
            t.dim,
        )));
    }
    if out.len() < height {
        out.push(Line::from(sp(
            clip(
                " Lanes show only after l reads a campaign; moving the cursor never reads.",
                width,
            ),
            t.dim,
        )));
    }
    out
}

/// One listed campaign's table row.
fn row(app: &App, f: &Fit, c: &CampaignRow, on: bool) -> Row<'static> {
    let (t, g) = (&app.theme, &app.data.glance);
    let gutter = match (on, app.changed.contains(&c.id)) {
        (true, _) => sp("▌", t.accent),
        (_, true) => sp("•", t.dim),
        _ => Span::raw(" "),
    };
    let (text, sub) = if c.parked {
        (t.dim, t.dim)
    } else {
        (t.text, t.sub)
    };
    let mark = if c.parked {
        sp("‖", t.dim)
    } else {
        ui::lead_mark(t, &c.lead, c.lead_waiting)
    };
    let mut lead = vec![gutter, mark];
    if f.state > 2 {
        lead.push(sp(format!(" {}", clip(state(c), f.state - 3)), sub));
    }
    let mut cells = vec![
        Cell::from(Line::from(lead)),
        Cell::from(sp(clip(&c.name, f.name), text)),
    ];
    if f.host > 0 {
        let host = if g.local(&c.host) {
            String::new()
        } else {
            ui::host_name(g, &c.host)
        };
        cells.push(Cell::from(sp(clip(&host, f.host), t.remote)));
    }
    cells.push(Cell::from(sp(counts(c), sub)));
    if f.activity > 0 {
        // A taskr older than P1b sends no buckets: unknown, not a quiet four hours.
        let bars = if c.spark.is_empty() {
            "?".to_string()
        } else {
            ui::spark(&c.spark, f.activity)
        };
        cells.push(Cell::from(sp(bars, t.accent)));
    }
    if f.milestone > 0 {
        // The owner note first, as the Dashboard's second line; `OWNER: nothing.` stripped.
        let note = one_line(c.status_text());
        cells.push(if note.is_empty() {
            Cell::from(sp("—", t.dim))
        } else {
            Cell::from(sp(clip(&note, f.milestone), sub))
        });
    }
    cells.push(Cell::from(sp(
        format!("{:>4}", age(c.activity_age_ms)),
        sub,
    )));
    let line = Row::new(cells);
    if on { line.style(t.selected()) } else { line }
}

/// The Fleet body: the roster over the cached lanes. The roster windows around the cursor,
/// so every row is reachable and the cursor's is always drawn; only drawn rows are hit
/// targets. Short of room the lanes keep one honest line.
pub(crate) fn body(f: &mut Frame, app: &App, area: Rect, tiny: bool) {
    let (t, g) = (&app.theme, &app.data.glance);
    let width = area.width as usize;
    let h = area.height as usize;
    if h == 0 || width == 0 {
        return;
    }
    let bottom = area.y + area.height;
    // The same identities the keys, the mouse and every restore use: active, then parked.
    let ids = glance::ids(app);
    let rows: Vec<Option<&CampaignRow>> = ids
        .iter()
        .map(|id| match id {
            RowId::Campaign(id) => g.campaigns.iter().find(|c| c.id == *id),
            _ => None,
        })
        .collect();
    let n = rows.len().max(1);
    let quiet = g.quiet.count > 0;
    // The column names, the rows, the quiet line and the totals.
    let want = 2 + n + usize::from(quiet);
    let pre = preview(app, width, usize::MAX).len();
    let full = h.saturating_sub(pre + 1).max(h / 2);
    let (roster, gap, below) = if !tiny && want.min(full) + 4 <= h {
        let roster = want.min(full);
        (roster, 1, pre.min(h - roster - 1))
    } else {
        let below = usize::from(h > 1);
        (want.min(h - below), 0, below)
    };
    let mut room = roster.saturating_sub(2);
    let quiet = quiet && (n < room || room >= 4);
    room = room.saturating_sub(usize::from(quiet)).max(1);
    let cursor = app.row.min(n - 1);
    let top = if n <= room {
        0
    } else {
        (cursor + 1).saturating_sub(room).min(n - room)
    };
    let window = top..(top + room).min(n);
    let line_at = |y: u16| Rect {
        y,
        height: 1,
        ..area
    };
    let mut y = area.y;
    let start = y;
    if rows.iter().all(Option::is_none) {
        // An empty roster's one inert stop: the cursor reaches it, nothing acts.
        let none = if app.filter.is_empty() {
            "No listed campaigns; not a fleet-wide idle claim"
        } else {
            "No campaign matches the filter"
        };
        let line = ui::row(
            t,
            vec![sp(none, t.sub)],
            app.row == 0,
            false,
            t.accent,
            width,
        );
        f.render_widget(Paragraph::new(line), line_at(y));
        app.hit(line_at(y), Hit::Row(0));
        y += 1;
    } else {
        let longest = g
            .campaigns
            .iter()
            .map(|c| c.name.width())
            .max()
            .unwrap_or(8);
        let remote = g
            .campaigns
            .iter()
            .any(|c| !g.local(&c.host) && !ui::host_name(g, &c.host).is_empty());
        let fit = Fit::new(width, longest, remote);
        let mut head = vec![if fit.state > 2 { " lead state" } else { "" }, "campaign"];
        let mut widths = vec![fit.state, fit.name];
        if fit.host > 0 {
            head.push("host");
            widths.push(fit.host);
        }
        head.push(if fit.lanes > 8 {
            "lanes w/r/o"
        } else {
            "w/r/o"
        });
        widths.push(fit.lanes);
        if fit.activity > 0 {
            head.push("activity 4h");
            widths.push(fit.activity);
        }
        if fit.milestone > 0 {
            head.push("last milestone");
            widths.push(fit.milestone);
        }
        head.push(" age");
        widths.push(4);
        let lines: Vec<Row> = window
            .clone()
            .map(|i| rows[i].map_or_else(Row::default, |c| row(app, &fit, c, i == app.row)))
            .collect();
        let table = Table::new(
            lines,
            widths.into_iter().map(|w| Constraint::Length(w as u16)),
        )
        .header(Row::new(head).style(Style::new().fg(t.dim)))
        .column_spacing(1);
        let height = (1 + window.len() as u16).min(bottom - y);
        f.render_widget(table, Rect { y, height, ..area });
        for (k, i) in window.clone().enumerate() {
            let at = y + 1 + k as u16;
            if at < bottom {
                app.hit(line_at(at), Hit::Row(i));
            }
        }
        y += height;
    }
    let mut put = |y: &mut u16, line: Line<'static>| {
        if *y < bottom {
            f.render_widget(Paragraph::new(line), line_at(*y));
            *y += 1;
        }
    };
    if quiet {
        let names = if g.quiet.names.is_empty() {
            String::new()
        } else {
            format!(": {}", g.quiet.names.join(" · "))
        };
        let text = format!(
            " quiet {}{names} · folded, not listed · c lists them",
            g.quiet.count
        );
        put(&mut y, Line::from(sp(clip(&text, width), t.dim)));
    }
    let at = if window.len() < rows.len() {
        format!("{}-{}/{} · ", window.start + 1, window.end, rows.len())
    } else {
        String::new()
    };
    put(&mut y, Line::from(sp(totals(app, width, &at), t.sub)));
    // A stale snapshot's roster dims, as the Dashboard's cards do; the banner says why.
    if app.fetch.loaded && app.fetch.error.is_some() {
        f.buffer_mut().set_style(
            Rect {
                y: start,
                height: y - start,
                ..area
            },
            Style::new().fg(t.dim),
        );
    }
    let y = y + gap as u16;
    if below > 0 && y < bottom {
        // All of it when it fits; else cut, or one line under three.
        let room = below.min((bottom - y) as usize);
        let lines = preview(app, width, if pre <= room { usize::MAX } else { room });
        let height = lines.len() as u16;
        f.render_widget(Paragraph::new(lines), Rect { y, height, ..area });
    }
}
