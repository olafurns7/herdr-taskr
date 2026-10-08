//! Shared drawing pieces: text fitting, the header with its verdict pill, the footer hints,
//! panes, charts and marks.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Paragraph},
};
use unicode_width::UnicodeWidthStr;

use crate::{App, Hit, theme::Theme};

/// Spinner frames from the spec's glyph set; the braille ones did not render.
const SPINNER: [&str; 4] = ["◐", "◓", "◑", "◒"];
const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

pub(crate) fn clip(text: &str, max: usize) -> String {
    if text.width() <= max {
        return text.to_string();
    }
    // Measured as a string, as everything else is: `⚠️` is one column char by char, two as
    // a string.
    let mut out = String::new();
    for ch in text.chars() {
        out.push(ch);
        if out.width() + 1 > max {
            out.pop();
            break;
        }
    }
    if max > 0 {
        out.push('…');
    }
    out
}

pub(crate) fn pad(text: &str, width: usize) -> String {
    let text = clip(text, width);
    let gap = width.saturating_sub(text.width());
    text + &" ".repeat(gap)
}

/// Ledger text on one line: line breaks and tabs become spaces.
pub(crate) fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Greedy word wrap. A word longer than the line is cut.
pub(crate) fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = vec![];
    for para in text.split('\n') {
        let mut line = String::new();
        for word in para.split_whitespace() {
            let mut word = word.to_string();
            if !line.is_empty() && line.width() + 1 + word.width() > width {
                out.push(std::mem::take(&mut line));
            }
            while word.width() > width {
                let head: String = word.chars().take(width).collect();
                word = word.chars().skip(width).collect();
                out.push(head);
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line += &word;
        }
        out.push(line);
    }
    out
}

/// `wrap`, cut to `max` lines; the last one ends in `…` when text is left over.
pub(crate) fn wrap_max(text: &str, width: usize, max: usize) -> Vec<String> {
    let mut lines = wrap(text, width);
    if lines.len() > max {
        lines.truncate(max);
        if let Some(last) = lines.last_mut() {
            *last = clip(&format!("{last} …"), width);
        }
    }
    lines
}

pub(crate) fn age(ms: i64) -> String {
    // A clock ahead on another host is "now", not a negative age.
    match (ms / 1000).max(0) {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

/// `HH:MM` of an RFC 3339 timestamp.
pub(crate) fn hhmm(at: &str) -> &str {
    at.get(11..16).unwrap_or("")
}

/// A short machine label for a taskr host name.
// ponytail: the last dash-separated part ("sam-laptop" -> "laptop"). Use Herdr's machine
// labels once the snapshot carries them.
fn host_label(host: &str) -> &str {
    host.rsplit('-').next().unwrap_or(host)
}

/// The machine a row is on: the hub's own name for the hub, a short label otherwise.
pub(crate) fn host_name(g: &crate::model::Glance, host: &str) -> String {
    if host.is_empty() {
        g.server_host.clone()
    } else {
        host_label(host).to_string()
    }
}

pub(crate) fn sp(text: impl Into<String>, fg: Color) -> Span<'static> {
    Span::styled(text.into(), Style::new().fg(fg))
}

pub(crate) fn bold(text: impl Into<String>, fg: Color) -> Span<'static> {
    Span::styled(
        text.into(),
        Style::new().fg(fg).add_modifier(Modifier::BOLD),
    )
}

pub(crate) fn width(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.content.width()).sum()
}

/// The spans cut to `max` columns.
pub(crate) fn fit(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
    let mut out = vec![];
    let mut left = max;
    for span in spans {
        let n = span.content.width();
        if n > left {
            if left > 0 {
                out.push(Span::styled(clip(&span.content, left), span.style));
            }
            break;
        }
        left -= n;
        out.push(span);
    }
    out
}

/// `left` at the start and `right` at the end of a line `width` wide; `left` gives way.
pub(crate) fn spread(
    left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
    width: usize,
) -> Vec<Span<'static>> {
    let right = fit(right, width);
    let mut out = fit(left, width.saturating_sub(self::width(&right)));
    out.push(Span::raw(" ".repeat(
        width.saturating_sub(self::width(&out) + self::width(&right)),
    )));
    out.extend(right);
    out
}

/// A list row with its one-column gutter: the cursor bar on the selected row, a dim dot
/// on a row that changed since the owner last looked.
pub(crate) fn row(
    t: &Theme,
    body: Vec<Span<'static>>,
    selected: bool,
    changed: bool,
    bar: Color,
    width: usize,
) -> Line<'static> {
    let gutter = match (selected, changed) {
        (true, _) => sp("▌", bar),
        (_, true) => sp("•", t.dim),
        _ => Span::raw(" "),
    };
    let mut spans = vec![gutter];
    spans.extend(spread(body, vec![], width.saturating_sub(1)));
    let line = Line::from(spans);
    if selected {
        line.style(t.selected())
    } else {
        line
    }
}

/// ` TITLE ───────── right `: a section rule with a count or a position.
pub(crate) fn rule(
    t: &Theme,
    title: &str,
    right: &str,
    color: Color,
    width: usize,
) -> Line<'static> {
    let fill = width.saturating_sub(title.width() + right.width() + 4);
    Line::from(vec![
        bold(format!(" {title} "), color),
        sp("─".repeat(fill), t.rule),
        sp(format!(" {right} "), t.dim),
    ])
}

pub(crate) fn panel(t: &Theme, title: &str, focused: bool) -> Block<'static> {
    let (border, fg) = if focused {
        (t.accent, t.text)
    } else {
        (t.rule, t.sub)
    };
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(border))
        .title(Line::from(bold(format!(" {title} "), fg)))
}

/// Draws a pane and returns its inside, one column in from each border.
pub(crate) fn pane(f: &mut Frame, t: &Theme, area: Rect, title: &str, focused: bool) -> Rect {
    let block = panel(t, title, focused);
    let inner = block.inner(area);
    f.render_widget(block, area);
    Rect {
        x: inner.x + 1,
        width: inner.width.saturating_sub(2),
        ..inner
    }
}

/// A scroll thumb on a pane's right border, when the content is longer than the view.
pub(crate) fn scrollbar(f: &mut Frame, t: &Theme, track: Rect, total: usize, top: usize) {
    let h = track.height as usize;
    if total <= h || h == 0 {
        return;
    }
    let len = (h * h / total).max(1);
    let start = (top * h / total).min(h - len);
    for y in start..start + len {
        f.buffer_mut()[(track.x, track.y + y as u16)]
            .set_symbol("█")
            .set_style(Style::new().fg(t.rule));
    }
}

/// Bars for `data`, `height` rows tall, top row first. One row keeps a baseline so an
/// idle stretch still reads as a line.
pub(crate) fn chart(data: &[u32], height: usize) -> Vec<String> {
    let top = data.iter().copied().max().unwrap_or(0).max(1) as usize;
    (0..height)
        .rev()
        .map(|r| {
            data.iter()
                .map(|&v| {
                    match (v as usize * height * 8)
                        .div_ceil(top)
                        .saturating_sub(r * 8)
                    {
                        0 if height == 1 => BARS[0],
                        0 => ' ',
                        level => BARS[level.min(8) - 1],
                    }
                })
                .collect()
        })
        .collect()
}

/// The last `n` buckets as a one-row sparkline.
pub(crate) fn spark(data: &[u32], n: usize) -> String {
    chart(&data[data.len().saturating_sub(n)..], 1).remove(0)
}

/// One `●` per lane: working, ready, then other open lanes as `○`; `—` for none.
pub(crate) fn pips(
    t: &Theme,
    lanes: &crate::model::LaneCounts,
    width: usize,
) -> Vec<Span<'static>> {
    let (work, ready, open) = (
        lanes.working as usize,
        lanes.ready as usize,
        lanes.open as usize,
    );
    if open == 0 {
        return vec![sp(pad("—", width), t.dim)];
    }
    let kinds = [
        (work, '●', t.work),
        (ready, '●', t.done),
        (open.saturating_sub(work + ready), '○', t.dim),
    ];
    let room = if open > width { width - 1 } else { width };
    let mut out = vec![];
    let mut used = 0;
    for (n, mark, color) in kinds {
        let n = n.min(room - used);
        used += n;
        out.push(sp(mark.to_string().repeat(n), color));
    }
    out.push(sp(
        if open > width {
            "+".to_string()
        } else {
            " ".repeat(width - used)
        },
        t.dim,
    ));
    out
}

/// The lead mark: `◐` working, `○` waiting on its lanes, `×` gone or blocked, `·` idle.
pub(crate) fn lead_mark(t: &Theme, lead: &str, waiting: bool) -> Span<'static> {
    match (lead, waiting) {
        ("working", _) => sp("◐", t.work),
        ("gone" | "blocked", _) => sp("×", t.check),
        (_, true) => sp("○", t.sub),
        _ => sp("·", t.dim),
    }
}

fn pill(t: &Theme, text: String, color: Color) -> Span<'static> {
    Span::styled(text, t.pill(color))
}

/// The verdict: the only filled colour block on the screen, then what else is waiting.
fn verdict(app: &App) -> (Span<'static>, Option<Span<'static>>) {
    let t = &app.theme;
    let g = &app.data.glance;
    let (need, check) = app.counts.unwrap_or((g.needs_you.len(), g.attention.len()));
    let needs = || bold(format!(" {need} need you"), t.need);
    if !app.fetch.loaded {
        // No snapshot yet: nothing to claim, least of all "no owner action".
        return match app.fetch.error {
            Some(_) => (pill(t, " ! no data ".into(), t.check), None),
            None => (sp("reading the ledger", t.sub), None),
        };
    }
    if app.fetch.error.is_some() {
        // Never a green header on data that may be old.
        let stale = pill(t, format!(" ! stale {} ", age(app.fetch.age_ms)), t.check);
        return (stale, (need > 0).then(needs));
    }
    if need > 0 {
        let extra = (check > 0).then(|| sp(format!(" {check} to check"), t.check));
        return (pill(t, format!(" ● {need} need you "), t.need), extra);
    }
    if check > 0 {
        return (pill(t, format!(" ! {check} to check "), t.check), None);
    }
    (pill(t, " ✓ no owner action ".into(), t.ok), None)
}

/// Under 30 columns or 6 rows there is room for the verdict and nothing else.
pub(crate) fn pill_only(f: &mut Frame, app: &App) {
    let area = f.area();
    f.render_widget(
        Paragraph::new(Line::from(verdict(app).0)),
        Rect {
            height: area.height.min(1),
            ..area
        },
    );
}

/// One line: `taskr ❯ crumb`, then the data age and the verdict pill. The pill always
/// fits; the hub name, the second count and the age give way in that order.
pub(crate) fn header(f: &mut Frame, app: &App, area: Rect, crumb: Vec<Span<'static>>) {
    let t = &app.theme;
    let w = area.width as usize;
    let mut left = vec![bold(" taskr", t.accent)];
    if !crumb.is_empty() {
        left.push(sp(" ❯ ", t.dim));
        left.extend(crumb);
    }
    if app.typing || !app.filter.is_empty() {
        left.push(sp(format!("  filter: {}", app.filter), t.accent));
        if app.typing {
            left.push(Span::styled(" ", t.cursor()));
        }
    }
    let (pill, extra) = verdict(app);
    let fetch = &app.fetch;
    let fresh = if fetch.in_flight {
        sp(SPINNER[fetch.tick % SPINNER.len()], t.sub)
    } else if fetch.error.is_some() {
        Span::raw("")
    } else {
        sp(
            format!("{} ago", age(fetch.age_ms)),
            // Pushed data is current until the hub says otherwise; polled data ages.
            if fetch.age_ms > 15_000 && fetch.live != Some(true) {
                t.check
            } else {
                t.dim
            },
        )
    };
    let right = |hub: bool, fresh_on: bool, extra_on: bool| {
        let mut v = vec![Span::raw("  ")];
        if hub && w >= 100 {
            v.push(sp(format!("hub {} · ", app.data.glance.server_host), t.dim));
        }
        if fresh_on && !fresh.content.is_empty() {
            v.extend([fresh.clone(), Span::raw("  ")]);
        }
        v.push(pill.clone());
        if let (true, Some(extra)) = (extra_on, &extra) {
            v.push(extra.clone());
        }
        v.push(Span::raw(" "));
        v
    };
    let right = [
        (true, true, true),
        (false, true, true),
        (false, true, false),
    ]
    .into_iter()
    .map(|(a, b, c)| right(a, b, c))
    .find(|v| width(v) + width(&left) <= w)
    .unwrap_or_else(|| right(false, false, false));
    f.render_widget(Paragraph::new(Line::from(spread(left, right, w))), area);
}

/// The hint line. Hints drop from the right when they do not fit, but the last one
/// (`? help`) stays.
pub(crate) fn footer(f: &mut Frame, app: &App, area: Rect, hints: &[(&str, &str)]) {
    let t = &app.theme;
    let shown = |n: usize| {
        hints[..n]
            .iter()
            .chain(hints.last().filter(|_| n < hints.len()))
    };
    let size = |n: usize| -> usize {
        1 + shown(n)
            .map(|(k, l)| width(&[Span::raw(format!("{k} {l}   "))]))
            .sum::<usize>()
    };
    // At the right, how the view learns of changes: pushed by the hub, or polling.
    let mode = match app.fetch.live {
        Some(true) => "live ",
        Some(false) => "polling ",
        None => "",
    };
    let room = (area.width as usize).saturating_sub(mode.len());
    let n = (0..=hints.len())
        .rev()
        .find(|&n| size(n) <= room + 3)
        .unwrap_or(0);
    let mut spans = vec![Span::raw(" ")];
    for (key, label) in shown(n) {
        // A click on a hint is its key.
        let x = area.x + width(&spans) as u16;
        let hint = [bold(*key, t.accent), sp(format!(" {label}   "), t.dim)];
        let w = (width(&hint) as u16).saturating_sub(3);
        app.hit(
            Rect {
                x,
                width: w,
                ..area
            },
            Hit::Key((*key).to_string()),
        );
        spans.extend(hint);
    }
    // The last hint's padding is not worth an ellipsis.
    if let Some(last) = spans.last_mut() {
        last.content = last.content.trim_end().to_string().into();
    }
    f.render_widget(
        Paragraph::new(Line::from(spread(
            fit(spans, room),
            vec![sp(mode, t.dim)],
            area.width as usize,
        ))),
        area,
    );
}

/// `--ascii`: every glyph as its ASCII stand-in, for a font or a link that lacks them.
pub(crate) fn ascii(buf: &mut ratatui::buffer::Buffer) {
    for cell in &mut buf.content {
        let plain = match cell.symbol() {
            s if s.is_ascii() => continue,
            "●" | "◐" | "◓" | "◑" | "◒" | "◆" => "*",
            "○" => "o",
            "•" | "·" | "░" | "…" | "▂" | "▃" => ".",
            "×" | "✗" | "⊘" => "x",
            "✓" => "v",
            "❯" | "↗" | "⏎" | "↳" => ">",
            "‹" => "<",
            "‖" | "▌" | "│" => "|",
            "─" | "—" | "▄" | "▅" => "-",
            "╭" | "╮" | "╰" | "╯" => "+",
            "▁" => "_",
            "▆" | "▇" => "=",
            "█" => "#",
            _ => "?",
        };
        cell.set_symbol(plain);
    }
}

/// Emoji sequences as their first character: `⚠️` as `⚠`, `👩‍💻` as `👩`. For a VS16
/// sequence ratatui's diff also writes the cell after it, and the backend prints that cell
/// without a cursor move; a terminal that advances two columns for the sequence (tmux 3.4,
/// Herdr's libghostty) puts it one column right, so the rest of the row shifts and old
/// glyphs and colours stay on screen. A lone character takes that path out; where it is
/// narrower than the sequence, the column it leaves is the blank the buffer already holds.
/// VS15 stays: it keeps a wide base such as `⌚︎` one column wide.
pub(crate) fn plain_emoji(buf: &mut ratatui::buffer::Buffer) {
    // VS16, ZWJ, the keycap mark, skin tones and tag characters.
    let joins = |c: char| {
        matches!(c, '\u{fe0f}' | '\u{200d}' | '\u{20e3}')
            || ('\u{1f3fb}'..='\u{1f3ff}').contains(&c)
            || ('\u{e0020}'..='\u{e007f}').contains(&c)
    };
    for cell in &mut buf.content {
        let symbol = cell.symbol();
        if symbol.chars().skip(1).any(joins) {
            let first: String = symbol.chars().take(1).collect();
            cell.set_symbol(&first);
        }
    }
}

/// Dims everything drawn so far, for a dialog or a stale frame.
pub(crate) fn dim(f: &mut Frame, area: Rect, fg: Color) {
    let buf = f.buffer_mut();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buf[(x, y)].set_style(
                Style::new()
                    .fg(fg)
                    .bg(Color::Reset)
                    .remove_modifier(Modifier::BOLD),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emoji_sequences_draw_as_one_character() {
        let mut buf = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 8, 1));
        for (text, drawn) in [
            ("⚠️", "⚠"),
            ("👩‍💻", "👩"),
            ("1️⃣", "1"),
            ("⌚︎", "⌚︎"),
            ("信", "信"),
        ] {
            buf.set_string(0, 0, text, Style::new());
            plain_emoji(&mut buf);
            assert_eq!(buf[(0, 0)].symbol(), drawn, "{text:?}");
            // Never wider than the cell the layout gave it.
            assert!(drawn.width() <= text.width(), "{text:?}");
        }
    }

    #[test]
    fn wide_text_fits_its_width() {
        for text in [
            "⌚︎ watch",
            "⚠️impl-frames",
            "impl-信頼-rules",
            "👩‍💻 pairing",
            "✅docs-refresh",
        ] {
            for max in 0..=text.width() + 1 {
                assert!(clip(text, max).width() <= max, "{text:?} at {max}");
                assert_eq!(pad(text, max).width(), max.max(clip(text, max).width()));
            }
        }
    }

    #[test]
    fn text_fits_its_width() {
        assert_eq!(clip("campaign", 5), "camp…");
        assert_eq!(pad("ab", 4), "ab  ");
        assert_eq!(
            wrap("one two three\n\nabcdefgh", 7),
            ["one two", "three", "", "abcdefg", "h"]
        );
        assert_eq!(wrap_max("one two three four", 7, 2), ["one two", "three …"]);
        assert_eq!(one_line("a\n b\tc"), "a b c");
        assert_eq!(age(41_000), "41s");
        assert_eq!(age(7_440_000), "2h");
    }

    #[test]
    fn charts_and_pips() {
        assert_eq!(spark(&[0, 4, 8], 3), "▁▄█");
        assert_eq!(chart(&[0, 8, 16], 2), ["  █", " ██"]);
        let t = crate::theme::DARK;
        let text = |w, r, o| -> String {
            let lanes = crate::model::LaneCounts {
                working: w,
                ready: r,
                open: o,
            };
            pips(&t, &lanes, 7)
                .iter()
                .map(|s| s.content.as_ref())
                .collect()
        };
        assert_eq!(text(0, 0, 0), "—      ");
        assert_eq!(text(1, 1, 3), "●●○    ");
        assert_eq!(text(5, 2, 9), "●●●●●●+");
    }
}
