//! The overlays: answering an ask (choose or write, then confirm the exact text) and the
//! help. A centred dialog over the dimmed glance from 60 columns; full screen under that.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Padding, Paragraph},
};

use crate::{
    App, Screen, ask, glance,
    keys::{self, GROUPS, KEYS},
    model::Need,
    theme::Theme,
    ui::{self, age, bold, sp, wrap},
};

/// Dims the screen and opens a centred dialog; returns its inside.
fn modal(f: &mut Frame, t: &Theme, width: u16, height: u16, title: &str) -> Rect {
    let area = f.area();
    // The header keeps its colour: the verdict pill shows on every screen.
    ui::dim(
        f,
        Rect {
            y: area.y + 1,
            height: area.height.saturating_sub(1),
            ..area
        },
        t.rule,
    );
    let (width, height) = (width.min(area.width), height.min(area.height));
    let rect = Rect {
        x: (area.width - width) / 2,
        y: (area.height - height) / 2,
        width,
        height,
    };
    f.render_widget(Clear, rect);
    let block = ui::panel(t, title, true)
        .style(Style::new().bg(t.panel))
        .padding(Padding::new(2, 2, 1, 1));
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    inner
}

/// `label  text`, wrapped with the text hanging under itself.
fn field(
    label: &str,
    text: &str,
    width: usize,
    label_style: Style,
    style: Style,
) -> Vec<Line<'static>> {
    let indent = label.chars().count();
    let lines = wrap(text, width.saturating_sub(indent));
    let row = |(i, l): (usize, String)| {
        let lead = if i == 0 {
            label.to_string()
        } else {
            " ".repeat(indent)
        };
        Line::from(vec![
            Span::styled(lead, label_style),
            Span::styled(l, style),
        ])
    };
    lines.into_iter().enumerate().map(row).collect()
}

fn where_from(app: &App, a: &Need, pane: bool) -> Span<'static> {
    let (t, g) = (&app.theme, &app.data.glance);
    let mut place = ui::host_name(g, &a.host);
    if pane {
        place = format!("{place} {}", a.pane_id);
    }
    if g.local(&a.host) {
        sp(place, t.sub)
    } else {
        sp(format!("{place} ↗"), t.remote)
    }
}

/// Step 1: the ask, its options as rows, "write your own answer", and the editable text.
/// `budget` caps the ask's own text so the options always stay on screen.
fn choose(app: &App, a: &Need, width: usize, budget: usize) -> Vec<Line<'static>> {
    let t = &app.theme;
    let parsed = ask::parse(&a.text);
    let mut rows: Vec<Line> = vec![];
    for (i, o) in parsed.options.iter().enumerate() {
        let on = i == app.choice;
        let mut body = wrap(&o.text, width.saturating_sub(7));
        let mut tag = o.recommended;
        if tag
            && body
                .last()
                .is_some_and(|l| l.chars().count() + 13 > width.saturating_sub(7))
        {
            body.push(String::new());
        }
        let n = body.len();
        for (j, text) in body.into_iter().enumerate() {
            let mut line = vec![
                sp(if on && j == 0 { " ❯ " } else { "   " }, t.accent),
                bold(
                    if j == 0 {
                        format!("{}  ", o.key)
                    } else {
                        "   ".into()
                    },
                    t.accent,
                ),
                sp(text, t.text),
            ];
            if tag && j + 1 == n {
                line.push(sp(
                    if line[2].content.is_empty() {
                        "recommended"
                    } else {
                        "  recommended"
                    },
                    t.ok,
                ));
                tag = false;
            }
            let line = Line::from(ui::spread(line, vec![], width));
            rows.push(if on { line.style(t.selected()) } else { line });
        }
    }
    let own = parsed.options.len();
    rows.push(Line::from(vec![
        sp(if app.choice >= own { " ❯ " } else { "   " }, t.accent),
        bold("…  ", t.accent),
        sp("write your own answer", t.sub),
    ]));
    rows.extend([Line::raw(""), Line::from(sp("─".repeat(width), t.rule))]);
    // Choosing an option fills the field with what it meant, so the ledger records it.
    let mut text = field(
        " answer  ",
        &answer_text(app, a),
        width.saturating_sub(1),
        Style::new().fg(t.dim),
        Style::new().fg(t.text),
    );
    if let Some(last) = text.last_mut() {
        last.spans.push(Span::styled(" ", t.cursor()));
    }
    rows.extend(text);

    let context = wrap(&parsed.context, width);
    let room = budget.saturating_sub(rows.len() + 1).max(1);
    let total = context.len();
    let mut out: Vec<Line> = if total <= room {
        app.seen.borrow_mut().page = (0, 0);
        context
            .into_iter()
            .map(|l| Line::from(sp(l, t.text)))
            .collect()
    } else {
        // A window on the text, and under it where the window is.
        let shown = room.saturating_sub(1).max(1);
        app.seen.borrow_mut().page = (shown, total);
        let first = app.scroll.min(total - shown);
        let mut out: Vec<Line> = context
            .into_iter()
            .skip(first)
            .take(shown)
            .map(|l| Line::from(sp(l, t.text)))
            .collect();
        let at = format!(
            "… {}-{}/{total} · ctrl-d ctrl-u scroll",
            first + 1,
            first + shown
        );
        out.push(Line::from(sp(at, t.dim)));
        out
    };
    out.push(Line::raw(""));
    out.extend(rows);
    out
}

fn answer_text(app: &App, _a: &Need) -> String {
    app.text.clone()
}

/// Step 2: where the answer goes, the exact text, and the exact command. Only `y` sends.
fn confirm(app: &App, a: &Need, width: usize) -> Vec<Line<'static>> {
    let t = &app.theme;
    let text = answer_text(app, a);
    let asker = if a.asker_task_id == a.root_id {
        format!("{} lead", a.campaign)
    } else {
        a.asker.clone()
    };
    let host = where_from(app, a, true).content.into_owned();
    let waiting = if a.asker_waiting {
        "waiting for this answer"
    } else {
        "not waiting: the answer is also sent to its pane"
    };
    // Shown as a shell line for the owner to read; the view runs it without a shell.
    let prompt = if a.asker_waiting { "" } else { " --prompt" };
    let command = format!(
        "taskr answer {} \"{}\"{prompt}",
        a.ask_id,
        text.replace('"', "\\\"")
    );
    let (dim, plain) = (Style::new().fg(t.dim), Style::new().fg(t.text));
    let mut out: Vec<Line> = wrap(
        "You are about to send this answer. It cannot be changed or sent twice.",
        width,
    )
    .into_iter()
    .map(|l| Line::from(sp(l, t.sub)))
    .collect();
    out.push(Line::raw(""));
    out.extend(field(
        "to    ",
        &format!("{asker} · {host} · {waiting}"),
        width,
        dim,
        plain,
    ));
    out.extend(field(
        "text  ",
        &text,
        width,
        dim,
        t.selected().fg(t.text).add_modifier(Modifier::BOLD),
    ));
    out.push(Line::raw(""));
    out.extend(field(
        "runs  ",
        &command,
        width,
        dim,
        Style::new().fg(t.sub),
    ));
    out
}

fn keys_line(t: &Theme, hints: &[(&str, &str)]) -> Line<'static> {
    let mut out = vec![];
    for (key, label) in hints {
        out.extend([
            bold(format!(" {key} "), t.accent),
            sp(format!("{label}   "), t.sub),
        ]);
    }
    Line::from(out)
}

pub(crate) fn answer(f: &mut Frame, app: &App) {
    let t = &app.theme;
    let Some(a) = app.ask.as_ref() else {
        return glance::draw(f, app);
    };
    let confirming = app.screen == Screen::Confirm;
    let hints: &[(&str, &str)] = if confirming {
        &[
            ("y", "send"),
            ("e", "edit"),
            ("esc", "cancel, nothing is sent"),
        ]
    } else if f.area().width < 60 {
        &[
            ("j k", "choose"),
            ("tab", "edit"),
            ("⏎", "review"),
            ("esc", "cancel"),
        ]
    } else {
        &[
            ("j k", "choose"),
            ("tab", "edit the text"),
            ("⏎", "review before sending"),
            ("esc", "cancel"),
        ]
    };
    let title = format!("ask {} · {}", a.ask_id, a.campaign);

    if f.area().width >= 60 {
        let mut background = app.clone();
        background.screen = Screen::Glance;
        glance::draw(f, &background);
        // Nothing under a dialog can be clicked.
        app.seen.borrow_mut().hits.clear();
        let inner = modal(
            f,
            t,
            80,
            if confirming { 16 } else { 20 },
            &format!("Answer {title}"),
        );
        let (w, h) = (inner.width as usize, inner.height as usize);
        let mut lines = if confirming {
            confirm(app, a, w)
        } else {
            choose(app, a, w, h - 2)
        };
        lines.extend([Line::raw(""), keys_line(t, hints)]);
        f.render_widget(Paragraph::new(lines), inner);
        return;
    }

    // Under 60 columns the dialog takes the whole pane; the verdict pill stays in the header.
    let [head, body, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(f.area());
    ui::header(f, app, head, vec![sp("answer", t.dim)]);
    ui::footer(f, app, foot, hints);
    let inner = Rect {
        x: body.x + 1,
        width: body.width - 2,
        ..body
    };
    let (w, h) = (inner.width as usize, inner.height as usize);
    // The pane id gives way when the line is full; the confirm step shows it.
    let state = |pane: bool| {
        vec![
            if a.blocking {
                bold("BLOCKING", t.need)
            } else {
                sp("not blocking", t.sub)
            },
            sp(
                if a.asker_waiting {
                    " · waiting · "
                } else {
                    " · not waiting · "
                },
                t.dim,
            ),
            sp(format!("{} · ", age(a.age_ms)), t.sub),
            where_from(app, a, pane),
        ]
    };
    let state = if ui::width(&state(true)) <= w {
        state(true)
    } else {
        state(false)
    };
    let mut lines = vec![
        Line::from(bold(title, t.text)),
        Line::from(ui::fit(state, w)),
        Line::raw(""),
    ];
    lines.extend(if confirming {
        confirm(app, a, w)
    } else {
        choose(app, a, w, h - 3)
    });
    f.render_widget(Paragraph::new(lines), inner);
}

/// The marks, as the views draw them.
fn legend(t: &Theme) -> Vec<Line<'static>> {
    let row = |items: &[(&str, ratatui::style::Color, &str)]| {
        let mut out = vec![Span::raw(" ")];
        for (mark, color, label) in items {
            out.extend([
                bold(format!(" {mark}"), *color),
                sp(format!(" {label}  "), t.text),
            ]);
        }
        Line::from(out)
    };
    vec![
        row(&[
            ("?", t.need, "ask"),
            ("!", t.check, "to check"),
            ("◐", t.work, "working"),
            ("○", t.sub, "waiting"),
        ]),
        row(&[
            ("●", t.work, "lane"),
            ("●", t.done, "ready"),
            ("·", t.dim, "idle"),
            ("×", t.check, "gone"),
            ("‖", t.dim, "parked"),
        ]),
        row(&[
            ("✓", t.done, "done"),
            ("⊘", t.dim, "abandoned"),
            ("✗", t.sub, "rejected"),
        ]),
        row(&[
            ("⏎", t.accent, "pane on this machine"),
            ("↗", t.remote, "another"),
        ]),
        row(&[("•", t.dim, "changed since you looked")]),
    ]
}

/// One group of the key table for a view, under its name.
fn group(t: &Theme, name: &str, view: u8, width: usize) -> Vec<Line<'static>> {
    let mut out = vec![Line::from(bold(format!(" {name}"), t.sub))];
    for key in KEYS
        .iter()
        .filter(|k| k.group == name && k.views & view != 0)
    {
        out.push(Line::from(vec![
            bold(format!("  {:<14}", key.keys), t.accent),
            sp(key.help, t.text),
        ]));
    }
    if name == "Go" {
        out.extend(
            wrap(keys::GO_NOTE, width.saturating_sub(2))
                .into_iter()
                .map(|l| Line::from(sp(format!("  {l}"), t.dim))),
        );
    }
    out.push(Line::raw(""));
    out
}

pub(crate) fn help(f: &mut Frame, app: &App) {
    let t = &app.theme;
    let (view, name) = match app.under() {
        Screen::Campaign => (keys::CAMPAIGN, "campaign"),
        Screen::Pager => (keys::PAGER, "pager"),
        _ => (keys::GLANCE, "glance"),
    };
    let marks = |mut lines: Vec<Line<'static>>| {
        lines.push(Line::from(bold(" Marks", t.sub)));
        lines.extend(legend(t));
        lines
    };
    if f.area().width >= 100 {
        let mut background = app.clone();
        background.screen = Screen::Glance;
        glance::draw(f, &background);
        // Nothing under a dialog can be clicked.
        app.seen.borrow_mut().hits.clear();
        let inner = modal(f, t, 100, 24, &format!("Keys · {name}"));
        let [a, b] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .areas(inner);
        let column = |names: &[&str]| {
            names
                .iter()
                .flat_map(|n| group(t, n, view, a.width as usize))
                .collect::<Vec<_>>()
        };
        f.render_widget(Paragraph::new(column(&GROUPS[..2])), a);
        f.render_widget(Paragraph::new(marks(column(&GROUPS[2..]))), b);
        return;
    }
    // One scrolling column.
    let [head, top, body, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(f.area());
    ui::header(f, app, head, vec![sp("help", t.dim)]);
    ui::footer(
        f,
        app,
        foot,
        &[
            keys::hint("j k", Some("scroll")),
            keys::hint("h", Some("close")),
        ],
    );
    let w = body.width as usize;
    let lines = marks(GROUPS.iter().flat_map(|n| group(t, n, view, w)).collect());
    let (total, h) = (lines.len(), body.height as usize);
    app.seen.borrow_mut().page = (h, total);
    let first = app.scroll.min(total.saturating_sub(h));
    let position = if total > h {
        format!("{}-{}/{total}", first + 1, first + h)
    } else {
        String::new()
    };
    f.render_widget(
        Paragraph::new(ui::rule(t, &format!("KEYS · {name}"), &position, t.sub, w)),
        top,
    );
    f.render_widget(
        Paragraph::new(lines.into_iter().skip(first).take(h).collect::<Vec<_>>()),
        body,
    );
}
