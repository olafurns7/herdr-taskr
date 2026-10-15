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
    App, Hit, Screen, ask, glance,
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
/// When the rows are short the spacing gives way first, then the scroll marks (folded
/// onto one row when that fits), then the context; a size that cannot hold the focused
/// option, the answer and the hints draws an explicit too-small body instead.
fn choose(
    app: &App,
    a: &Need,
    width: usize,
    budget: usize,
) -> (Vec<Line<'static>>, Vec<Option<usize>>) {
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
    // The window on the options keeps the chosen one; "write your own answer" keeps the
    // last. What must always show is the focused option, the answer row with its cursor
    // and the hints (the caller draws those): when the rows are short, the spacing gives
    // way first — the gap above the options, then the blank above the rule — then the
    // scroll marks, folded onto one row when that fits, then the context. A size that
    // still cannot hold the essentials draws an explicit too-small body instead of a
    // dialog that hides its answer.
    let tail_min = 3; // the own-answer row, the rule, the answer with its cursor
    let mut spacing = 2; // the blank rows: above the options, then above the rule
    let (mut first, mut shown) = (0, n);
    let mut folded = false;
    let mut mark_rows = 0;
    let mut too_small = false;
    if short && n > 0 {
        while spacing > 0 && budget < tail_min + spacing + n {
            spacing -= 1;
        }
        if budget < tail_min + spacing + n {
            spacing = 0;
            let (a, b) = (n - 1, n - 1);
            folded = format!("   ↑ {a} more · ↓ {b} more").width() <= width;
            mark_rows = if folded { 1 } else { 2 };
            loop {
                match budget.checked_sub(tail_min + mark_rows) {
                    Some(room) if room >= 1 => {
                        shown = room.min(n);
                        break;
                    }
                    _ if mark_rows == 0 => {
                        too_small = true;
                        break;
                    }
                    _ => mark_rows = 0,
                }
            }
            first = app
                .choice
                .min(n - 1)
                .saturating_sub(shown - 1)
                .min(n - shown);
        }
    } else if short {
        // No options: the tail alone, the spacing giving way until it fits.
        while spacing > 0 && budget < tail_min + spacing {
            spacing -= 1;
        }
        too_small = budget < tail_min;
    }
    if too_small {
        app.seen.borrow_mut().page = (0, 0);
        return (
            vec![Line::from(sp("window too small: resize to answer", t.sub))],
            vec![None],
        );
    }
    let gap_above = usize::from(spacing >= 2);
    let more = |count: usize, arrow: &str| {
        (count > 0).then(|| Line::from(sp(format!("   {arrow} {count} more"), t.dim)))
    };
    let (mut above, mut below): (Vec<Line<'static>>, Vec<Line<'static>>) = (vec![], vec![]);
    if shown < n && mark_rows > 0 {
        let (hidden_above, hidden_below) = (first, n - first - shown);
        if folded && hidden_above > 0 && hidden_below > 0 {
            above.push(Line::from(sp(
                format!("   ↑ {hidden_above} more · ↓ {hidden_below} more"),
                t.dim,
            )));
        } else {
            above.extend(more(hidden_above, "↑"));
            below.extend(more(hidden_below, "↓"));
        }
    }
    let option_rows = labels[first..first + shown]
        .iter()
        .map(Vec::len)
        .sum::<usize>()
        + above.len()
        + below.len();

    let own = parsed.options.len();
    let mut tail = vec![Line::from(vec![
        sp(if app.choice >= own { " ❯ " } else { "   " }, t.accent),
        bold("…  ", t.accent),
        sp("write your own answer", t.sub),
    ])];
    if spacing >= 1 {
        tail.push(Line::raw(""));
    }
    tail.push(Line::from(sp("─".repeat(width), t.rule)));
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
        .saturating_sub(option_rows + tail.len() + gap_above + 2)
        .max(1);
    if text.len() > keep {
        text.drain(..text.len() - keep);
        text[0].spans[0] = Span::styled(" answer …", Style::new().fg(t.dim));
    }
    tail.extend(text);

    // Two lines stay for the ask's own text: one of it, and where the window is.
    let fixed = option_rows + tail.len() + gap_above + 2;
    let described = parsed.options[first..first + shown]
        .iter()
        .filter(|o| !o.description.is_empty())
        .count();
    let each = (budget.saturating_sub(fixed) / described.max(1)).min(3);

    let mut rows: Vec<Line> = above.into_iter().collect();
    // Which option each row belongs to, for a click.
    let mut marks: Vec<Option<usize>> = vec![None; rows.len()];
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
        marks.extend(std::iter::repeat_n(Some(i), lines.len()));
        rows.extend(
            lines
                .into_iter()
                .map(|l| if on { l.style(t.selected()) } else { l }),
        );
    }
    marks.extend(std::iter::repeat_n(None, below.len()));
    rows.extend(below);
    // "Write your own answer" is a choice too; the rule and the answer are not.
    marks.push(Some(own));
    marks.extend(std::iter::repeat_n(None, tail.len() - 1));
    rows.extend(tail);

    let mut context = wrap(&parsed.context, width);
    // The answer's row and its cursor are always reserved, so when the rows run past
    // the budget the context gives way first: its window's last line and the position
    // indicator share one row, or the context drops to zero rows.
    let room = budget.saturating_sub(rows.len() + gap_above);
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
    if spacing >= 2 {
        out.push(Line::raw(""));
    }
    let mut at = vec![None; out.len()];
    at.extend(marks);
    out.extend(rows);
    (out, at)
}

/// The review in `room` lines: a long one scrolls (`ctrl-d ctrl-u`, the wheel), its last
/// line saying which lines show.
fn review(app: &App, lines: Vec<Line<'static>>, room: usize) -> Vec<Line<'static>> {
    let total = lines.len();
    if total <= room {
        app.seen.borrow_mut().page = (room, total);
        return lines;
    }
    let shown = room.saturating_sub(1).max(1);
    app.seen.borrow_mut().page = (shown, total);
    let first = app.scroll.min(total - shown);
    let mut out: Vec<Line> = lines.into_iter().skip(first).take(shown).collect();
    out.push(Line::from(sp(
        format!(
            "{}-{}/{total} · ctrl-d ctrl-u scroll",
            first + 1,
            first + shown
        ),
        app.theme.dim,
    )));
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
    let intro = match (app.refused(), app.resend, app.gone) {
        (Some(reason), ..) => format!(
            "The ledger did not record this answer: {reason}. It is kept here to read; it is not \
             sent again."
        ),
        (_, true, true) => "Whether this answer reached the ledger is unknown, and the ask is no \
                            longer open: nothing is resent."
            .into(),
        (_, true, false) => "Whether this answer reached the ledger is unknown, and the ask is \
                             still open. y sends it again unchanged, with the same request key."
            .into(),
        _ => "You are about to send this answer. It cannot be changed or sent twice.".into(),
    };
    let mut out: Vec<Line> = wrap(&intro, width)
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
    // Only keys that do something here: a refused answer or a resend whose ask closed can
    // only be closed; a draft whose ask closed can still be edited.
    let hints: &[(&str, &str)] =
        if confirming && (app.refused().is_some() || (app.resend && app.gone)) {
            &[("esc", "close")]
        } else if confirming && app.gone {
            &[("e", "edit"), ("esc", "close")]
        } else if confirming && app.resend {
            &[("y", "resend"), ("esc", "keep it")]
        } else if confirming {
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
        crate::background(f, app);
        // Nothing under a dialog can be clicked.
        app.seen.borrow_mut().hits.clear();
        let inner = modal(
            f,
            t,
            // 80x24 when the pane has room; `modal` shrinks it to fit a smaller one.
            80,
            24,
            &format!("Answer {title}"),
        );
        let (w, h) = (inner.width as usize, inner.height as usize);
        let mut lines = gone(app, a, w);
        let top = lines.len();
        let (body, marks) = if confirming {
            (
                review(app, confirm(app, a, w), h.saturating_sub(2 + top)),
                vec![],
            )
        } else {
            choose(app, a, w, h.saturating_sub(2 + top))
        };
        lines.extend(body);
        lines.extend([Line::raw(""), keys_line(t, hints)]);
        options(app, inner, top, &marks);
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
    ];
    lines.extend(gone(app, a, w));
    lines.push(Line::raw(""));
    let top = lines.len();
    let (body, marks) = if confirming {
        (
            review(app, confirm(app, a, w), h.saturating_sub(top)),
            vec![],
        )
    } else {
        choose(app, a, w, h.saturating_sub(top))
    };
    lines.extend(body);
    options(app, inner, top, &marks);
    f.render_widget(Paragraph::new(lines), inner);
}

/// The dialog's ask left a successful snapshot: answered or withdrawn elsewhere.
fn gone(app: &App, a: &Need, width: usize) -> Vec<Line<'static>> {
    if !app.gone {
        return vec![];
    }
    wrap(
        &format!(
            "! ask {} is no longer open (answered or withdrawn elsewhere); y sends nothing, \
             esc closes",
            a.ask_id
        ),
        width,
    )
    .into_iter()
    .map(|l| Line::from(bold(l, app.theme.check)))
    .collect()
}

/// A click on an option's rows chooses it; it never sends.
fn options(app: &App, inner: Rect, top: usize, marks: &[Option<usize>]) {
    for (i, mark) in marks.iter().enumerate() {
        let y = top + i;
        if let Some(o) = mark
            && y < inner.height as usize
        {
            app.hit(
                Rect {
                    y: inner.y + y as u16,
                    height: 1,
                    ..inner
                },
                Hit::Opt(*o),
            );
        }
    }
}

/// The asks picker: every open owner ask, whatever the screen under it shows, then the
/// refused answers kept to read whose ask is no longer open. `j k` choose, `⏎` answers the
/// highlighted ask or reads the kept answer, `A` shows an ask on the Asks screen, `esc`
/// closes.
pub(crate) fn picker(f: &mut Frame, app: &App) {
    let t = &app.theme;
    let asks = app.pickable();
    let picked = app.picked().map(|a| a.ask_id);
    let (n, open) = (asks.len(), app.data.glance.needs_you.len());
    let title = match open {
        0 => "Asks".to_string(),
        1 => "Asks · 1 open owner ask".to_string(),
        n => format!("Asks · {n} open owner asks"),
    };
    let title = match n - open {
        0 => title,
        1 => format!("{title} · 1 kept answer"),
        k => format!("{title} · {k} kept answers"),
    };
    let hints: &[(&str, &str)] = if n == 0 {
        &[("A", "Asks"), ("esc", "close")]
    } else if app.picked_kept() {
        &[("j k", "choose"), ("⏎", "read"), ("esc", "close")]
    } else {
        &[
            ("j k", "choose"),
            ("⏎", "answer"),
            ("A", "Asks"),
            ("esc", "close"),
        ]
    };
    let stale = app.fetch.error.as_ref().filter(|_| app.fetch.loaded);
    let wide = f.area().width >= 60;
    let (inner, foot) = if wide {
        crate::background(f, app);
        // Nothing under a dialog can be clicked.
        app.seen.borrow_mut().hits.clear();
        let extra = usize::from(stale.is_some()) as u16 + u16::from(n > open);
        let height = (2 * n.clamp(2, 12) as u16 + 6 + extra).min(24);
        (modal(f, t, 66, height, &title), 2)
    } else {
        let [head, body, foot] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(f.area());
        ui::header(f, app, head, vec![sp("asks", t.dim)]);
        ui::footer(f, app, foot, hints);
        let inner = Rect {
            x: body.x + 1,
            width: body.width.saturating_sub(2),
            ..body
        };
        f.render_widget(
            Paragraph::new(Line::from(bold(title.clone(), t.text))),
            inner,
        );
        (
            Rect {
                y: inner.y + 1,
                height: inner.height.saturating_sub(1),
                ..inner
            },
            0,
        )
    };
    let w = inner.width as usize;
    let mut lines: Vec<Line> = vec![];
    if let Some(error) = stale {
        lines.push(Line::from(sp(
            ui::clip(&format!("! {error} · the last snapshot"), w),
            t.check,
        )));
    }
    // The kept answers have a label line of their own.
    let label = usize::from(n > open);
    let room = (inner.height as usize).saturating_sub(lines.len() + foot + label);
    if n == 0 {
        let (first, second) = if app.fetch.loaded {
            ("Nothing needs you", "No open owner ask in this snapshot.")
        } else {
            (
                "No ledger snapshot yet",
                "The asks show when the ledger answers.",
            )
        };
        lines.extend([Line::from(sp(first, t.text)), Line::from(sp(second, t.sub))]);
    } else {
        // A window of two-line asks that keeps the highlighted one.
        let fits = if 2 * n <= room {
            n
        } else {
            (room.saturating_sub(1) / 2).clamp(1, n)
        };
        let at = asks
            .iter()
            .position(|a| Some(a.ask_id) == picked)
            .unwrap_or(0);
        let first = (at + 1).saturating_sub(fits).min(n - fits);
        for (i, a) in asks.iter().enumerate().skip(first).take(fits) {
            if i == open {
                lines.push(Line::from(sp(
                    ui::clip("NOT RECORDED · kept to read, no longer open", w),
                    t.check,
                )));
            }
            let y = inner.y + lines.len() as u16;
            if y < inner.bottom() {
                app.hit(
                    Rect {
                        y,
                        height: 2.min(inner.bottom() - y),
                        ..inner
                    },
                    Hit::Ask(a.ask_id),
                );
            }
            lines.extend(glance::ask_lines(app, a, Some(a.ask_id) == picked, w));
        }
        if fits < n {
            let more = format!("{}-{}/{n} · j k for the rest", first + 1, first + fits);
            lines.push(Line::from(sp(more, t.dim)));
        }
    }
    if wide {
        let pad = (inner.height as usize).saturating_sub(lines.len() + 1);
        lines.extend(std::iter::repeat_n(Line::raw(""), pad));
        lines.push(keys_line(t, hints));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// The marks, as the views draw them; prose wraps to `width`.
fn legend(t: &Theme, width: usize) -> Vec<Line<'static>> {
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
    let mut out = vec![
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
        Line::from(bold("  My PRs", t.sub)),
        row(&[
            ("✗", t.need, "failing"),
            ("◷", t.check, "pending"),
            ("?", t.sub, "not known"),
        ]),
        row(&[("✓", t.ok, "checks passing"), ("□", t.sub, "draft")]),
    ];
    // Each rule starts its own lines, so `req ✗` never wraps apart.
    for rule in [
        "N+? at least N, more unread.",
        "req ✗ only when a check GitHub marks required failed on the head; a required ✓ is never shown.",
    ] {
        out.extend(
            wrap(rule, width.saturating_sub(3))
                .into_iter()
                .map(|l| Line::from(sp(format!("  {l}"), t.text))),
        );
    }
    out
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
        "Act" if view & (keys::GLANCE | keys::CAMPAIGN) != 0 => Some(keys::ANSWER_NOTE),
        _ => None,
    };
    // The wide help's left column has the room for it.
    let ledger = (name == "Go" && view == keys::GLANCE).then_some(keys::LEDGER_NOTE);
    let screens = (name == "Go" && view & (keys::GLANCE | keys::LIST | keys::DETAIL) != 0)
        .then_some(keys::SCREENS_NOTE);
    for note in note.into_iter().chain(screens).chain(ledger) {
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
        Screen::Asks => (keys::LIST, "asks"),
        Screen::Fleet => (keys::LIST, "fleet"),
        Screen::Detail => (keys::DETAIL, "detail"),
        _ => (keys::GLANCE, "dashboard"),
    };
    let marks = |mut lines: Vec<Line<'static>>, width: u16| {
        lines.push(Line::from(bold(" Marks", t.sub)));
        if view == keys::SLOTR {
            lines.extend(slotr_legend(t, width as usize));
        } else {
            lines.extend(legend(t, width as usize));
        }
        lines
    };
    if f.area().width >= 100 {
        crate::background(f, app);
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
        let (left, right) = (column(&GROUPS[..2]), marks(column(&GROUPS[2..]), b.width));
        // Both columns scroll together, so every key and mark is reachable in the modal.
        let (total, h) = (left.len().max(right.len()), inner.height as usize);
        app.seen.borrow_mut().page = (h, total);
        let first = app.scroll.min(total.saturating_sub(h));
        if total > h {
            let at = format!("Keys · {name} · {}-{}/{total} · j k", first + 1, first + h);
            modal(f, t, 100, 30, &at);
        }
        let shown = |lines: Vec<Line<'static>>| {
            Paragraph::new(lines.into_iter().skip(first).take(h).collect::<Vec<_>>())
        };
        f.render_widget(shown(left), a);
        f.render_widget(shown(right), b);
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
        if text.contains("window too small") {
            // The explicit too-small body (clipped at 30 columns): never a dialog that
            // hides its answer.
            assert!(
                !text.contains(" answer "),
                "{case}: the too-small body still pretends\n{text}"
            );
            return;
        }
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
            // Under 46 columns the footer cannot hold every hint; it keeps esc.
            if width >= 46 {
                assert!(foot.contains(hint), "{case}: hint {hint:?} cut\n{text}");
            }
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
        for (width, height) in [
            (46, 12),
            (40, 10),
            (30, 8),
            (46, 20),
            (46, 30),
            (70, 30),
            (120, 40),
        ] {
            for (name, labels) in [("ascii", &ascii), ("cjk", &cjk), ("short", &short)] {
                for count in [2, 4] {
                    for recommended in [false, true] {
                        for description in ["", long.as_str()] {
                            for multi in [false, true] {
                                for note in [false, true] {
                                    for focus in [count - 1, count / 2] {
                                        let mut app = App::new(frames::fixture());
                                        let ask = asking(
                                            &labels[..count],
                                            recommended,
                                            description,
                                            multi,
                                        );
                                        app.answer(ask);
                                        if multi {
                                            for i in 0..count {
                                                app.choose(i);
                                                app.toggle();
                                            }
                                        }
                                        app.choose(focus);
                                        if note {
                                            app.edit(|n| {
                                                *n = format!("{}END", "note ".repeat(80))[3..]
                                                    .to_string()
                                            });
                                        }
                                        let case = format!(
                                            "{width}x{height} {name} {count} rec={recommended} \
                                             desc={} multi={multi} note={note} focus={focus}",
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
        }
        assert_eq!(cases, 1344);
    }

    /// A plain ask whose text lists `count` `(A)`-style options, as a long heuristic
    /// list does.
    fn plain(count: usize) -> Need {
        let mut ask = frames::structured_asks()[2].clone();
        ask.question = None;
        ask.text = (0..count)
            .map(|i| format!("({}) Option {i}", char::from(b'A' + i as u8)))
            .collect::<Vec<_>>()
            .join("; ");
        ask
    }

    #[test]
    fn a_window_keeps_both_marks_the_choice_and_the_answer() {
        // Twelve plain options with the focus in the middle: both scroll marks show, and
        // the focused option, the answer row with its cursor and the hints stay too.
        for (width, height, up, down) in [
            (46, 12, "↑ 4 more", "↓ 5 more"),
            (40, 10, "↑ 6 more", "↓ 5 more"),
        ] {
            let mut app = App::new(frames::fixture());
            app.answer(plain(12));
            app.choose(6);
            let buf = render(&app, width, height);
            let text = frames::text(&buf);
            println!("MIDDLE-OPTION {width}x{height}\n{text}");
            assert!(
                text.contains(up) && text.contains(down),
                "{width}x{height}: a scroll mark is lost\n{text}"
            );
            assert!(
                text.contains("❯ G  Option 6"),
                "{width}x{height}: focus lost\n{text}"
            );
            // The cursor is drawn only on the answer's row: if it is on screen, the
            // answer row with it is.
            let cursor = (0..height)
                .flat_map(|y| (0..width).map(move |x| (x, y)))
                .find(|&p| buf[p].style().bg == Some(app.theme.text))
                .unwrap_or_else(|| panic!("{width}x{height}: the cursor is clipped\n{text}"));
            let row = text.lines().nth(cursor.1 as usize).unwrap_or("");
            assert!(
                row.contains(" answer ") && row.contains("Option 6"),
                "{width}x{height}: the cursor left the answer\n{text}"
            );
            assert!(text.contains("esc cancel"), "{width}x{height}:\n{text}");
        }
    }

    #[test]
    fn a_windowed_ask_has_ascii_marks() {
        // The window's arrows have ASCII stand-ins: ↑ as ^ and ↓ as v.
        let mut app = App::new(frames::fixture());
        app.answer(plain(12));
        app.choose(6);
        let text = frames::text(&render(&app, 46, 12));
        assert!(text.contains("↑ 4 more · ↓ 5 more"), "{text}");
        app.ascii = true;
        let text = frames::text(&render(&app, 46, 12));
        assert!(text.is_ascii(), "{text}");
        assert!(text.contains("^ 4 more . v 5 more"), "{text}");
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
