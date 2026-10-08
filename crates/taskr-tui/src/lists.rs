//! The all-campaigns list (every root, closed ones dimmed) and the pager for a document.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::{
    App, Hit,
    keys::hint,
    model::RootRow,
    theme::Theme,
    ui::{self, age, bold, clip, pad, sp, wrap},
};

fn root_line(app: &App, r: &RootRow, wide: bool, width: usize) -> Vec<Span<'static>> {
    let (t, g) = (&app.theme, &app.data.glance);
    let closed = r.status == "closed";
    let live = g.campaigns.iter().find(|c| c.id == r.id && !closed);
    let mark = match (closed, r.parked, live) {
        (true, ..) => sp("✓", t.dim),
        (_, true, _) => sp("‖", t.dim),
        (_, _, Some(c)) => ui::lead_mark(t, &c.lead, c.lead_waiting),
        _ => sp("·", t.dim),
    };
    let (fg, sub) = if closed || r.parked {
        (t.dim, t.dim)
    } else {
        (t.text, t.sub)
    };
    let host = if r.host == g.caller_host {
        String::new()
    } else {
        ui::host_name(g, &r.host)
    };
    let lanes = if closed {
        r.lanes_total.to_string()
    } else {
        format!("{}/{}", r.lanes_open, r.lanes_total)
    };
    if !wide {
        let left = vec![mark, sp(format!(" {}", r.name), fg)];
        let right = vec![
            sp(format!(" {lanes:>5} "), sub),
            sp(format!("{host:<4}"), t.remote),
            sp(format!("{:>4}", age(r.activity_age_ms)), sub),
        ];
        return ui::spread(left, right, width);
    }
    let status = if closed {
        "closed"
    } else if r.parked {
        "parked"
    } else {
        "open"
    };
    let bars = live
        .filter(|_| !r.parked)
        .map_or(String::new(), |c| ui::spark(&c.spark, 24));
    let left = vec![
        mark,
        sp(format!(" {}", pad(&r.name, 30)), fg),
        sp(format!(" #{:<6}", r.id), t.dim),
        sp(format!("{status:<8}"), sub),
        sp(
            format!(
                "{:<17}",
                if closed {
                    format!("{lanes} lanes")
                } else {
                    format!("{lanes} lanes open")
                }
            ),
            sub,
        ),
        sp(format!("{host:<8}"), t.remote),
        sp(bars, t.accent),
    ];
    ui::spread(
        left,
        vec![sp(format!(" {:>4} ago ", age(r.activity_age_ms)), sub)],
        width,
    )
}

/// The roots in list order: open ones, then closed ones.
pub(crate) fn roots(app: &App) -> Vec<&RootRow> {
    let (open, closed): (Vec<&RootRow>, Vec<&RootRow>) =
        app.data.roots.iter().partition(|r| r.status != "closed");
    open.into_iter().chain(closed).collect()
}

/// Every root: open ones, then closed ones in a window that shows its position.
pub(crate) fn all_campaigns(f: &mut Frame, app: &App) {
    let t = &app.theme;
    let area = f.area();
    let wide = area.width >= 100;
    let [head, body, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);
    ui::header(f, app, head, vec![sp("all campaigns", t.dim)]);
    ui::footer(
        f,
        app,
        foot,
        &[
            hint("⏎", None),
            hint("l", None),
            hint("/", Some("search")),
            hint("h", None),
            hint("?", None),
        ],
    );

    let (open, closed): (Vec<&RootRow>, Vec<&RootRow>) =
        app.data.roots.iter().partition(|r| r.status != "closed");
    let mut list = body;
    if wide {
        let block = ui::panel(
            t,
            &format!(
                "All campaigns · {} open · {} closed",
                open.len(),
                closed.len()
            ),
            true,
        );
        list = block.inner(body);
        f.render_widget(block, body);
    }
    let (w, h) = (list.width as usize, list.height as usize);
    // One window over every root, kept around the cursor; each rule says which of its
    // rows are showing.
    let all: Vec<&RootRow> = open.iter().chain(&closed).copied().collect();
    let rows = h.saturating_sub(2);
    let first = (app.row + 1)
        .saturating_sub(rows)
        .min(all.len().saturating_sub(rows));
    let last = (first + rows).min(all.len());
    let split = open.len();
    let shown = |from: usize, to: usize, of: usize| {
        if to - from == of {
            of.to_string()
        } else if to == from {
            format!("0/{of}")
        } else {
            format!("{}-{}/{of}", from + 1, to)
        }
    };
    let row = |i: usize| {
        ui::row(
            t,
            root_line(app, all[i], wide, w - 1),
            i == app.row,
            false,
            t.accent,
            w,
        )
    };
    app.seen.borrow_mut().rows = all.len();
    for i in first..last {
        // One rule above the open rows, two above the closed ones.
        let y = list.y + (i - first) as u16 + if i < split { 1 } else { 2 };
        let line = Rect {
            y,
            height: 1,
            ..list
        };
        app.hit(line, Hit::Row(i));
    }
    let mut lines = vec![ui::rule(
        t,
        "OPEN",
        &shown(first.min(split), last.min(split), split),
        t.sub,
        w,
    )];
    lines.extend((first.min(split)..last.min(split)).map(row));
    let (from, to) = (first.max(split), last.max(split));
    lines.push(ui::rule(
        t,
        "CLOSED",
        &shown(from - split, to - split, closed.len()),
        t.dim,
        w,
    ));
    lines.extend((from..to).map(row));
    f.render_widget(Paragraph::new(lines), list);
}

/// A document as lines: headings, bullets with hanging indents, code and tables unwrapped.
fn markdown(t: &Theme, body: &str, width: usize) -> Vec<Line<'static>> {
    let mut out = vec![];
    let mut code = false;
    for raw in body.lines() {
        if raw.starts_with("```") {
            code = !code;
            continue;
        }
        if code || raw.starts_with('|') {
            if !raw.starts_with("|-") {
                out.push(Line::from(sp(clip(&format!("  {raw}"), width), t.sub)));
            }
            continue;
        }
        let text = raw.replace("**", "");
        let level = text.chars().take_while(|&c| c == '#').count();
        if level > 0 {
            let color = [t.accent, t.text, t.sub][level.min(3) - 1];
            out.extend(
                wrap(text[level..].trim(), width)
                    .into_iter()
                    .map(|l| Line::from(bold(l, color))),
            );
            continue;
        }
        // A list item hangs under its own text: "- " or "12. ".
        let digits = text.chars().take_while(char::is_ascii_digit).count();
        let (lead, rest) = match text.strip_prefix("- ") {
            Some(rest) => ("• ".to_string(), rest),
            None if digits > 0 && text[digits..].starts_with(". ") => {
                (text[..digits + 2].to_string(), &text[digits + 2..])
            }
            None => (String::new(), text.as_str()),
        };
        let indent = lead.chars().count();
        for (i, line) in wrap(rest, width.saturating_sub(indent))
            .into_iter()
            .enumerate()
        {
            let lead = if i == 0 {
                lead.clone()
            } else {
                " ".repeat(indent)
            };
            out.push(Line::from(vec![sp(lead, t.accent), sp(line, t.text)]));
        }
    }
    out
}

/// A report or a plan, read in place. Space pages; `q` closes.
pub(crate) fn pager(f: &mut Frame, app: &App) {
    let (t, d) = (&app.theme, &app.data.doc);
    let area = f.area();
    let wide = area.width >= 100;
    let [head, body, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);
    let name = if d.name.is_empty() { &d.lane } else { &d.name };
    let title = format!("{} · {name} · v{}", d.kind, d.version);
    let crumb = if wide {
        vec![
            bold(app.data.campaign.root.name.clone(), t.text),
            sp(" ❯ ", t.dim),
            sp(d.kind.clone(), t.dim),
        ]
    } else {
        vec![sp(d.kind.clone(), t.dim)]
    };
    ui::header(f, app, head, crumb);
    ui::footer(
        f,
        app,
        foot,
        &[
            hint("space", None),
            hint("/", Some("search")),
            hint("y", None),
            hint("q", Some("close")),
        ],
    );

    let page = if wide {
        ui::pane(f, t, body, &title, true)
    } else {
        Rect {
            x: body.x + 1,
            y: body.y + 1,
            width: body.width - 2,
            height: body.height - 1,
        }
    };
    let lines = markdown(t, &d.body, (page.width as usize).min(100));
    let (total, h) = (lines.len(), page.height as usize);
    app.seen.borrow_mut().page = (h, total);
    let first = app.scroll.min(total.saturating_sub(h));
    let position = format!("{}-{}/{total}", first + 1, (first + h).min(total));
    if wide {
        let at = Line::from(sp(format!(" {position} "), t.sub)).right_aligned();
        f.render_widget(
            at,
            Rect {
                y: body.y,
                height: 1,
                width: body.width - 2,
                ..body
            },
        );
        ui::scrollbar(
            f,
            t,
            Rect {
                x: body.right() - 1,
                y: page.y,
                width: 1,
                height: page.height,
            },
            total,
            first,
        );
    } else {
        f.render_widget(
            Paragraph::new(ui::rule(
                t,
                &clip(&title, body.width as usize - position.len() - 5),
                &position,
                t.text,
                body.width as usize,
            )),
            Rect { height: 1, ..body },
        );
    }
    f.render_widget(
        Paragraph::new(lines.into_iter().skip(first).take(h).collect::<Vec<_>>()),
        page,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_keeps_structure() {
        let t = crate::theme::DARK;
        let text = |body: &str| -> Vec<String> {
            let lines = markdown(&t, body, 12);
            lines
                .iter()
                .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
                .collect()
        };
        assert_eq!(
            text("## A **b**\n- one two three\n10. four five six"),
            ["A b", "• one two", "  three", "10. four", "    five six"]
        );
        assert_eq!(
            text("```\ncode stays unwrapped\n```\n|a|b|\n|-|-|"),
            ["  code stay…", "  |a|b|"]
        );
    }
}
