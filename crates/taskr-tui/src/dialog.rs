//! The overlays: answering an ask (choose or write, then confirm the exact text) and the
//! help. A centred dialog over the dimmed glance from 60 columns; full screen under that.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Padding, Paragraph},
};
use unicode_width::UnicodeWidthStr;

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
/// `budget` caps the ask's own text so the options always stay on screen. A structured
/// ask's descriptions share what is left, an equal number of lines each, or none.
///
/// The answer's row, the chosen option and the hints below always show: the options get
/// what is left, first with their labels wrapped, then one cut line each, then a window
/// with `↑ n more` and `↓ n more`; the ask's own text gets the rest, possibly nothing.
fn choose(app: &App, a: &Need, width: usize, budget: usize) -> Vec<Line<'static>> {
    let t = &app.theme;
    let parsed = ask::parse(a);
    // ` ❯ `, `[x] ` when several can be picked, `A  `, and a column to spare.
    let lead = if parsed.multi { 11 } else { 7 };
    let tag = if parsed.structured {
        "★ recommended"
    } else {
        "recommended"
    };
    let labels: Vec<Vec<String>> = parsed
        .options
        .iter()
        .map(|o| {
            let mut body = wrap(&o.text, width.saturating_sub(lead));
            if o.recommended
                && body
                    .last()
                    .is_some_and(|l| l.width() + tag.width() + 2 > width.saturating_sub(lead))
            {
                body.push(String::new());
            }
            body
        })
        .collect();
    // Around the options: the gap above them, "write your own answer", a blank, the rule
    // and one row of the answer.
    let around = 5;
    let n = labels.len();
    let short = labels.iter().map(Vec::len).sum::<usize>() + around > budget;
    let labels: Vec<Vec<String>> = if short {
        parsed
            .options
            .iter()
            .map(|o| vec![o.text.clone()])
            .collect()
    } else {
        labels
    };
    // The window on the options keeps the chosen one; "write your own answer" keeps the last.
    let (first, shown) = if short && n > 0 && n + around > budget {
        let shown = budget.saturating_sub(around + 2).clamp(1, n);
        let first = app
            .choice
            .min(n - 1)
            .saturating_sub(shown - 1)
            .min(n - shown);
        (first, shown)
    } else {
        (0, n)
    };
    let more = |count: usize, arrow: &str| {
        (count > 0).then(|| Line::from(sp(format!("   {arrow} {count} more"), t.dim)))
    };
    let (above, below) = (more(first, "↑"), more(n - first - shown, "↓"));
    let option_rows = labels[first..first + shown]
        .iter()
        .map(Vec::len)
        .sum::<usize>()
        + above.iter().len()
        + below.iter().len();

    let own = parsed.options.len();
    let mut tail = vec![Line::from(vec![
        sp(if app.choice >= own { " ❯ " } else { "   " }, t.accent),
        bold("…  ", t.accent),
        sp("write your own answer", t.sub),
    ])];
    tail.extend([Line::raw(""), Line::from(sp("─".repeat(width), t.rule))]);
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
    // The answer keeps its tail and the cursor in view; `…` marks the text above the window.
    let keep = budget
        .saturating_sub(option_rows + tail.len() + 1 + 2)
        .max(1);
    if text.len() > keep {
        text.drain(..text.len() - keep);
        text[0].spans[0] = Span::styled(" answer …", Style::new().fg(t.dim));
    }
    tail.extend(text);

    // Two lines stay for the ask's own text: one of it, and where the window is.
    let fixed = option_rows + tail.len() + 1 + 2;
    let described = parsed.options[first..first + shown]
        .iter()
        .filter(|o| !o.description.is_empty())
        .count();
    let each = (budget.saturating_sub(fixed) / described.max(1)).min(3);

    let mut rows: Vec<Line> = above.into_iter().collect();
    for ((i, o), body) in parsed
        .options
        .iter()
        .enumerate()
        .zip(labels)
        .skip(first)
        .take(shown)
    {
        let on = i == app.choice;
        let n = body.len();
        let mut lines = vec![];
        for (j, text) in body.into_iter().enumerate() {
            let mut line = vec![sp(if on && j == 0 { " ❯ " } else { "   " }, t.accent)];
            if parsed.multi {
                let mark = match (j, app.picked.contains(&i)) {
                    (0, true) => "[x] ",
                    (0, false) => "[ ] ",
                    _ => "    ",
                };
                line.push(bold(mark, t.accent));
            }
            line.push(bold(
                if j == 0 {
                    format!("{}  ", o.key)
                } else {
                    "   ".into()
                },
                t.accent,
            ));
            if short {
                // One cut line: the label gives way to the tag.
                line.push(sp(text, t.text));
                let right = if o.recommended {
                    vec![sp(format!("  {tag}"), t.ok)]
                } else {
                    vec![]
                };
                lines.push(Line::from(ui::spread(line, right, width)));
                continue;
            }
            if o.recommended && j + 1 == n {
                let gap = if text.is_empty() { "" } else { "  " };
                line.extend([sp(text, t.text), sp(format!("{gap}{tag}"), t.ok)]);
            } else {
                line.push(sp(text, t.text));
            }
            lines.push(Line::from(ui::spread(line, vec![], width)));
        }
        if !o.description.is_empty() {
            for l in ui::wrap_max(&o.description, width.saturating_sub(lead), each) {
                let line = vec![sp(" ".repeat(lead - 1), t.dim), sp(l, t.dim)];
                lines.push(Line::from(ui::spread(line, vec![], width)));
            }
        }
        rows.extend(
            lines
                .into_iter()
                .map(|l| if on { l.style(t.selected()) } else { l }),
        );
    }
    rows.extend(below);
    rows.extend(tail);

    let mut context = wrap(&parsed.context, width);
    // The answer's row and its cursor are always reserved, so when the rows run past
    // the budget the context gives way first: its window's last line and the position
    // indicator share one row, or the context drops to zero rows.
    let room = budget.saturating_sub(rows.len() + 1);
    let total = context.len();
    let mut out: Vec<Line> = if total <= room {
        app.seen.borrow_mut().page = (0, 0);
        context
            .into_iter()
            .map(|l| Line::from(sp(l, t.text)))
            .collect()
    } else if room >= 2 {
        // A window on the text, and under it where the window is.
        let shown = room - 1;
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
    } else if room == 1 {
        // One row for the window: its first line and where it is share it.
        app.seen.borrow_mut().page = (1, total);
        let first = app.scroll.min(total - 1);
        let at = format!(
            "… {}-{}/{total} · ctrl-d ctrl-u scroll",
            first + 1,
            first + 1
        );
        let line = context.remove(first);
        vec![Line::from(ui::spread(
            vec![sp(line, t.text)],
            vec![sp(at, t.dim)],
            width,
        ))]
    } else {
        // No room at all: the context waits behind ctrl-d.
        app.seen.borrow_mut().page = (0, total);
        vec![]
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
    let parsed = ask::parse(a);
    let tab = |long| match (parsed.structured, long) {
        (true, true) => "add a note",
        (true, false) => "note",
        (false, true) => "edit the text",
        (false, false) => "edit",
    };
    let (long, short) = if parsed.multi {
        (
            vec![
                ("j k", "choose"),
                ("space", "pick"),
                ("tab", tab(true)),
                ("⏎", "review"),
                ("esc", "cancel"),
            ],
            vec![
                ("space", "pick"),
                ("tab", tab(false)),
                ("⏎", "review"),
                ("esc", "cancel"),
            ],
        )
    } else {
        (
            vec![
                ("j k", "choose"),
                ("tab", tab(true)),
                ("⏎", "review before sending"),
                ("esc", "cancel"),
            ],
            vec![
                ("j k", "choose"),
                ("tab", tab(false)),
                ("⏎", "review"),
                ("esc", "cancel"),
            ],
        )
    };
    // A structured ask's long hints must fit the dialog's inside (its trailing gap aside);
    // a plain ask keeps the hints it has always had.
    let inside = f.area().width.min(80).saturating_sub(6) as usize;
    let fits =
        f.area().width >= 60 && (!parsed.structured || keys_line(t, &long).width() <= inside + 3);
    let hints: &[(&str, &str)] = if confirming {
        &[
            ("y", "send"),
            ("e", "edit"),
            ("esc", "cancel, nothing is sent"),
        ]
    } else if fits {
        &long
    } else {
        &short
    };
    let title = format!("ask {} · {}", a.ask_id, a.campaign);

    if f.area().width >= 60 {
        let mut background = app.clone();
        background.screen = Screen::Glance;
        // The glance behind keeps its detail pane's place; `scroll` is this overlay's.
        background.scroll = app.back.last().map_or(0, |b| b.scroll);
        glance::draw(f, &background);
        // Nothing under a dialog can be clicked.
        app.seen.borrow_mut().hits.clear();
        let inner = modal(
            f,
            t,
            80,
            // A structured ask's descriptions take more room.
            if confirming {
                16
            } else if parsed.structured {
                26
            } else {
                20
            },
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
        row(&[
            ("◆", t.accent, "structured ask"),
            ("◇", t.accent, "options from its text"),
        ]),
        row(&[
            ("★", t.ok, "recommended"),
            ("HUB", t.remote, "the hub's dialog"),
        ]),
    ]
}

/// The slotr view's marks.
fn slotr_legend(t: &Theme, width: usize) -> Vec<Line<'static>> {
    let mut out = vec![Line::from(vec![
        Span::raw(" "),
        bold(" ●", t.work),
        sp(" holds a slot  ", t.text),
        bold(" ○", t.sub),
        sp(" queued  ", t.text),
        bold(" ▲", t.accent),
        sp(" priority", t.text),
    ])];
    let amber =
        "amber: stopping, warned, yielding, overdue, or waiting on memory, psi, load, recovery";
    out.extend(
        wrap(amber, width.saturating_sub(3))
            .into_iter()
            .map(|l| Line::from(sp(format!("  {l}"), t.check))),
    );
    out
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
    let note = match name {
        "Go" => Some(keys::GO_NOTE),
        "Move" if view == keys::GLANCE => Some(keys::DETAIL_NOTE),
        "Act" if view & (keys::GLANCE | keys::CAMPAIGN) != 0 => Some(keys::ANSWER_NOTE),
        _ => None,
    };
    if let Some(note) = note {
        out.extend(
            wrap(note, width.saturating_sub(2))
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
        Screen::Slotr => (keys::SLOTR, "slotr"),
        _ => (keys::GLANCE, "glance"),
    };
    let marks = |mut lines: Vec<Line<'static>>, width: u16| {
        lines.push(Line::from(bold(" Marks", t.sub)));
        if view == keys::SLOTR {
            lines.extend(slotr_legend(t, width as usize));
        } else {
            lines.extend(legend(t));
        }
        lines
    };
    if f.area().width >= 100 {
        let mut background = app.clone();
        background.screen = Screen::Glance;
        // The glance behind keeps its detail pane's place; `scroll` is this overlay's.
        background.scroll = app.back.last().map_or(0, |b| b.scroll);
        glance::draw(f, &background);
        // Nothing under a dialog can be clicked or scrolled.
        *app.seen.borrow_mut() = crate::Seen::default();
        let inner = modal(f, t, 100, 30, &format!("Keys · {name}"));
        let [a, b] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .areas(inner);
        let column = |names: &[&str]| {
            names
                .iter()
                .flat_map(|n| group(t, n, view, a.width as usize))
                .collect::<Vec<_>>()
        };
        f.render_widget(Paragraph::new(column(&GROUPS[..2])), a);
        f.render_widget(Paragraph::new(marks(column(&GROUPS[2..]), b.width)), b);
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
    let lines = marks(
        GROUPS.iter().flat_map(|n| group(t, n, view, w)).collect(),
        body.width,
    );
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

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

    use crate::{
        App, draw, frames,
        model::{Need, QuestionOption},
    };

    fn render(app: &App, width: u16, height: u16) -> Buffer {
        let mut t = Terminal::new(TestBackend::new(width, height)).expect("a test backend");
        t.draw(|f| draw(f, app)).expect("a frame");
        t.backend().buffer().clone()
    }

    #[test]
    fn a_long_answer_keeps_its_tail_and_the_cursor_in_view() {
        // Four 60-character labels, all picked, and a long note, at 46x20.
        let mut app = App::new(frames::fixture());
        let mut ask = frames::structured_asks()[2].clone();
        let options = &mut ask.question.as_mut().expect("a question").options;
        for (i, o) in options.iter_mut().enumerate() {
            o.label = format!("{i}{}", " label".repeat(10))[..60].to_string();
        }
        app.answer(ask);
        for i in 0..4 {
            app.choose(i);
            app.toggle();
        }
        app.edit(|note| *note = format!("{}END", "note ".repeat(80)));
        let buf = render(&app, 46, 20);
        let text = frames::text(&buf);
        assert!(text.contains(" answer …"), "{text}");
        assert!(text.contains("❯ [x] D  3 label"), "{text}");
        assert!(text.contains("esc cancel"), "{text}");
        let (y, line) = text
            .lines()
            .enumerate()
            .find(|(_, l)| l.ends_with("END"))
            .unwrap_or_else(|| panic!("the answer's tail is on screen:\n{text}"));
        let x = line.chars().count() as u16;
        let cell = buf[(x, y as u16)].style();
        assert_eq!(
            cell,
            cell.patch(app.theme.cursor()),
            "the cursor follows the tail"
        );
    }

    #[test]
    fn a_recommended_label_still_leaves_the_answer_on_screen() {
        // The same four 60-character labels at 46x20, with the first one recommended and
        // a wrapped question: the context gives way, the answer and its cursor stay.
        let mut app = App::new(frames::fixture());
        let mut ask = frames::structured_asks()[2].clone();
        let question = ask.question.as_mut().expect("a question");
        for (i, o) in question.options.iter_mut().enumerate() {
            o.label = format!("{i}{}", " label".repeat(10))[..60].to_string();
            o.recommended = i == 0;
        }
        ask.text = format!(
            "Translations landed for three of the four locales. Which locales ship in the first release? {}",
            question
                .options
                .iter()
                .zip('A'..)
                .map(|(o, c)| format!("({c}) {}", o.label))
                .collect::<Vec<_>>()
                .join("; ")
        );
        app.answer(ask);
        for i in 0..4 {
            app.choose(i);
            app.toggle();
        }
        app.edit(|note| *note = format!("{}END", "note ".repeat(80)));
        let buf = render(&app, 46, 20);
        let text = frames::text(&buf);
        assert!(text.contains(" answer …"), "{text}");
        assert!(text.contains("esc cancel"), "{text}");
        let (y, line) = text
            .lines()
            .enumerate()
            .find(|(_, l)| l.ends_with("END"))
            .unwrap_or_else(|| panic!("the answer's tail is on screen:\n{text}"));
        let x = line.chars().count() as u16;
        let cell = buf[(x, y as u16)].style();
        assert_eq!(
            cell,
            cell.patch(app.theme.cursor()),
            "the cursor follows the tail"
        );
    }

    /// A structured ask with `labels`, the first `recommended`, each described by
    /// `description`, its text as Q1 writes it.
    fn asking(labels: &[String], recommended: bool, description: &str, multi: bool) -> Need {
        let mut ask = frames::structured_asks()[2].clone();
        let question = ask.question.as_mut().expect("a question");
        question.multi_select = multi;
        question.options = labels
            .iter()
            .enumerate()
            .map(|(i, label)| QuestionOption {
                label: label.clone(),
                description: description.into(),
                recommended: recommended && i == 0,
            })
            .collect();
        let tail = labels
            .iter()
            .zip('A'..)
            .map(|(l, c)| format!("({c}) {l}"))
            .collect::<Vec<_>>()
            .join("; ");
        ask.text = format!("Which locales ship in the first release? {tail}");
        ask
    }

    /// The answer row, the cursor (after `END` when the note has it), the chosen option and
    /// the complete hints are on screen; a visible recommended option shows its whole tag.
    fn holds(app: &App, width: u16, height: u16, end: bool, case: &str) {
        let buf = render(app, width, height);
        let text = frames::text(&buf);
        let lines: Vec<&str> = text.lines().collect();
        let ask = app.ask.as_ref().expect("an ask");
        let q = ask.question.as_ref().expect("a question");
        let answer = lines
            .iter()
            .position(|l| l.contains(" answer "))
            .unwrap_or_else(|| panic!("{case}: no answer row\n{text}"));
        let cursor = (0..height)
            .flat_map(|y| (0..width).map(move |x| (x, y)))
            .find(|&p| buf[p].style().bg == Some(app.theme.text))
            .unwrap_or_else(|| panic!("{case}: no cursor\n{text}"));
        assert!(
            cursor.1 as usize >= answer,
            "{case}: cursor above the answer\n{text}"
        );
        if end {
            let before: String = (cursor.0.saturating_sub(3)..cursor.0)
                .map(|x| buf[(x, cursor.1)].symbol())
                .collect();
            assert_eq!(
                before, "END",
                "{case}: the tail is not before the cursor\n{text}"
            );
        }
        let option = |i: usize| {
            let key = char::from(b'A' + i as u8);
            let pick = if q.multi_select { "[x] " } else { "" };
            let row = format!("{pick}{key}  ");
            lines
                .iter()
                .position(|l| l.contains(&format!("❯ {row}")) || l.contains(&format!("   {row}")))
        };
        let focused = option(app.choice).unwrap_or_else(|| panic!("{case}: focus hidden\n{text}"));
        assert!(lines[focused].contains('❯'), "{case}: focus mark\n{text}");
        let hints: &[&str] = if q.multi_select {
            &["space pick", "tab ", "⏎ review", "esc cancel"]
        } else {
            &["j k choose", "tab ", "⏎ review", "esc cancel"]
        };
        let foot = lines
            .iter()
            .rev()
            .find(|l| l.contains("esc"))
            .copied()
            .unwrap_or("");
        for hint in hints {
            assert!(foot.contains(hint), "{case}: hint {hint:?} cut\n{text}");
        }
        if q.options[0].recommended && option(0).is_some() {
            assert!(
                text.contains("★ recommended"),
                "{case}: the tag is cut\n{text}"
            );
        }
    }

    #[test]
    fn wide_labels_keep_the_answer_visible() {
        // The reviewer's case: four 58-character CJK labels, the first recommended, all
        // picked, at 46x20.
        let labels: Vec<String> = (0..4)
            .map(|i| format!("{i}{}", "界 ".repeat(29)).trim_end().to_string())
            .collect();
        let mut app = App::new(frames::fixture());
        app.answer(asking(&labels, true, "", true));
        for i in 0..4 {
            app.choose(i);
            app.toggle();
        }
        app.edit(|note| *note = "END".into());
        holds(&app, 46, 20, true, "wide labels");
    }

    #[test]
    fn every_valid_question_keeps_the_answer_the_choice_and_the_hints() {
        let ascii: Vec<String> = (0..4)
            .map(|i| format!("{i}{}", " label".repeat(10))[..60].to_string())
            .collect();
        let cjk: Vec<String> = (0..4)
            .map(|i| format!("{i}{}", "界 ".repeat(29)).trim_end().to_string())
            .collect();
        let short: Vec<String> = (0..4).map(|i| format!("Option {i}")).collect();
        let long = "lorem ipsum ".repeat(42)[..500].to_string();
        let mut cases = 0;
        for (width, height) in [(46, 20), (46, 30), (70, 30), (120, 40)] {
            for (name, labels) in [("ascii", &ascii), ("cjk", &cjk), ("short", &short)] {
                for count in [2, 4] {
                    for recommended in [false, true] {
                        for description in ["", long.as_str()] {
                            for multi in [false, true] {
                                for note in [false, true] {
                                    let mut app = App::new(frames::fixture());
                                    let ask =
                                        asking(&labels[..count], recommended, description, multi);
                                    app.answer(ask);
                                    if multi {
                                        for i in 0..count {
                                            app.choose(i);
                                            app.toggle();
                                        }
                                    }
                                    app.choose(count - 1);
                                    if note {
                                        app.edit(|n| {
                                            *n = format!("{}END", "note ".repeat(80))[3..]
                                                .to_string()
                                        });
                                    }
                                    let case = format!(
                                        "{width}x{height} {name} {count} rec={recommended} \
                                         desc={} multi={multi} note={note}",
                                        description.len()
                                    );
                                    holds(&app, width, height, note, &case);
                                    cases += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(cases, 384);
    }

    #[test]
    fn every_hint_shows_in_full() {
        for (width, height) in [(46, 20), (46, 30), (70, 30), (120, 40)] {
            for ask in 0..3 {
                let mut app = App::new(frames::fixture());
                app.answer(frames::structured_asks()[ask].clone());
                let text = frames::text(&render(&app, width, height));
                assert!(text.contains("esc cancel"), "{width}x{height}:\n{text}");
            }
        }
    }
}
