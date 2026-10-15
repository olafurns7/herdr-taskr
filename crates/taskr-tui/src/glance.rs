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

    // A section with rows keeps its rule even when its window is empty.
    let used = |total: usize, shown: usize| if total == 0 { 0 } else { 1 + 2 * shown };
    // The row under the cursor is drawn first: when a pane is too short for every
    // section's share, the other sections shrink to their rules.
    let rule = floor.min(1);
    let (in_asks, in_checks) = (cursor < asks, cursor >= asks && cursor < asks + checks);
    let room = height.saturating_sub(floor + if checks > 0 { 3 } else { 0 });
    let room = if in_asks && room < 3 {
        height.saturating_sub(rule + checks.min(1))
    } else {
        room
    };
    let ask_rows = window(asks, room, in_asks.then_some(cursor));
    let room = height.saturating_sub(floor + used(asks, ask_rows.len()));
    let room = if in_checks && room < 3 {
        room + floor - rule
    } else {
        room
    };
    let check_rows = window(
        checks,
        room,
        cursor.checked_sub(asks).filter(|&i| i < checks),
    );

    let rows =
        height.saturating_sub(used(asks, ask_rows.len()) + used(checks, check_rows.len()) + 1);
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

#[cfg(test)]
pub(crate) fn lines(app: &App, width: usize, height: usize) -> Vec<Line<'static>> {
    layout(app, width, height).0
}

/// The list's lines, and for each line the cursor row it belongs to.
fn layout(app: &App, width: usize, height: usize) -> (Vec<Line<'static>>, Vec<Option<usize>>) {
    let (t, g) = (&app.theme, &app.data.glance);
    let mut hits: Vec<Option<usize>> = vec![];
    let pr_height = external_height(app, height);
    let offset = g.needs_you.len() + g.attention.len();
    let external_count = external_count(app);
    let ledger_cursor = if app.row >= offset {
        app.row.saturating_sub(external_count)
    } else {
        app.row
    };
    let plan = plan(g, height.saturating_sub(pr_height), ledger_cursor);
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

    let pr_start = out.len();
    if pr_height > 0 {
        hits.resize(out.len(), None);
        let (lines, rows) = external_lines(app, width, pr_height, index);
        out.extend(lines);
        hits.extend(rows);
        index += external_count;
    }

    let pr_end = out.len();
    if plan.tail == Tail::None {
        dim_ledger(app, &mut out, pr_start..pr_end);
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
    // No bars at all (a taskr older than P1b): the names take the room.
    let spark_w = if width < 40 || active.iter().all(|c| c.spark.is_empty()) {
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
                let mut right = vec![sp(format!(" parked {:>3}", age(c.park_age_ms)), t.dim)];
                if let Some(label) = c.tmp_label()
                    && ui::width(&left) + ui::width(&right) + label.len() < body
                {
                    right.insert(0, sp(format!(" {label}"), t.dim));
                }
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
    dim_ledger(app, &mut out, pr_start..pr_end);
    (out, hits)
}

/// When section rules no longer fit, keep the selected identity visible in a scrolling
/// one-line list. Its full action target is unchanged.
fn compact_layout(
    app: &App,
    width: usize,
    height: usize,
) -> (Vec<Line<'static>>, Vec<Option<usize>>) {
    let ids = ids(app);
    let start = (app.row + 1)
        .saturating_sub(height)
        .min(ids.len().saturating_sub(height));
    let mut out = vec![];
    let mut hits = vec![];
    for i in (start..ids.len()).take(height) {
        let mut at = app.clone();
        at.row = i;
        let (label, color) = match selected(&at) {
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
                    "PR {}{} #{} {} @{}",
                    p.ci.word(),
                    if p.truncated { " truncated" } else { "" },
                    p.number,
                    p.title,
                    p.author
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
                        if app.shipped.sources.iter().any(|s| s.error.is_some()) {
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
                    "Build {} {} · {} · latest100 sample",
                    r.label(),
                    r.workflow,
                    r.branch
                ),
                app.theme.sub,
            ),
            Selected::Nothing => continue,
        };
        let mut line = ui::row(
            &app.theme,
            vec![sp(clip(&label, width.saturating_sub(1)), color)],
            i == app.row,
            false,
            color,
            width,
        );
        if !matches!(ids[i], RowId::Pr(..) | RowId::Author(_) | RowId::Run(..))
            && app.fetch.loaded
            && app.fetch.error.is_some()
        {
            for span in &mut line.spans {
                span.style.fg = Some(app.theme.dim);
            }
        }
        out.push(line);
        hits.push(Some(i));
    }
    (out, hits)
}

fn dim_ledger(app: &App, lines: &mut [Line<'_>], prs: Range<usize>) {
    if app.fetch.loaded && app.fetch.error.is_some() {
        for (i, line) in lines.iter_mut().enumerate() {
            if !prs.contains(&i) {
                for span in &mut line.spans {
                    span.style.fg = Some(app.theme.dim);
                }
            }
        }
    }
}

/// Full panel demand and the header/status/overflow minimum retained for later kinds.
fn external_budget(app: &App, kind: crate::github::Kind) -> (usize, usize) {
    use crate::github::Kind;
    let sources = &kind.dashboard(app).sources;
    if sources.is_empty() {
        return (0, 0);
    }
    let base = 2
        + sources.len()
        + usize::from(kind == Kind::Build)
        + if kind == Kind::Build {
            0
        } else {
            sources.iter().filter(|s| s.total == 100).count()
        };
    let count = match kind {
        Kind::Open => app.github.rows().len(),
        Kind::Shipped => app.shipped.authors(app.observed_at).len(),
        Kind::Build => app.builds.runs().len(),
    };
    let g = &app.data.glance;
    let cap = if kind == Kind::Open
        && g.needs_you.is_empty()
        && g.attention.is_empty()
        && g.campaigns.is_empty()
    {
        usize::MAX
    } else {
        3
    };
    let per_row = if kind == Kind::Shipped { 1 } else { 2 };
    let empty =
        usize::from(count == 0 && kind != Kind::Open && sources.iter().any(|s| s.at.is_some()));
    (
        base + per_row * count.min(cap) + usize::from(count > cap) + empty,
        base + usize::from(count > 0) + empty,
    )
}

fn external_count(app: &App) -> usize {
    app.github.rows().len() + app.shipped.authors(app.observed_at).len() + app.builds.runs().len()
}
fn external_height(app: &App, height: usize) -> usize {
    let g = &app.data.glance;
    let floor = usize::from(!g.needs_you.is_empty()) * 3
        + usize::from(!g.attention.is_empty()) * 3
        + usize::from(!g.campaigns.is_empty()) * 2;
    let floor = if floor == 0 { 8 } else { floor };
    let wanted: usize = [
        crate::github::Kind::Open,
        crate::github::Kind::Shipped,
        crate::github::Kind::Build,
    ]
    .into_iter()
    .map(|kind| external_budget(app, kind).0)
    .sum();
    let available = wanted.min(height.saturating_sub(floor));
    // A two-line row plus its overflow hint may leave one unusable line; return it to the ledger.
    external_lines(app, 1, available, g.needs_you.len() + g.attention.len())
        .0
        .len()
}
fn external_lines(
    app: &App,
    width: usize,
    height: usize,
    offset: usize,
) -> (Vec<Line<'static>>, Vec<Option<usize>>) {
    use crate::github::Kind;
    let mut kinds: Vec<_> = [Kind::Open, Kind::Shipped, Kind::Build]
        .into_iter()
        .filter(|k| !k.dashboard(app).sources.is_empty())
        .collect();
    let mut minimum = 0;
    let keep = kinds
        .iter()
        .take_while(|kind| {
            minimum += external_budget(app, **kind).1;
            minimum <= height
        })
        .count()
        .max(1);
    kinds.truncate(keep);
    let mut out = vec![];
    let mut hits = vec![];
    let mut offset = offset;
    let mut remaining = height;
    for (i, kind) in kinds.iter().enumerate() {
        let later_min: usize = kinds[i + 1..]
            .iter()
            .map(|kind| external_budget(app, *kind).1)
            .sum();
        let share = external_budget(app, *kind)
            .0
            .min(remaining.saturating_sub(later_min));
        let (lines, rows) = if *kind == Kind::Open {
            pr_lines(app, width, share, offset)
        } else {
            observed_lines(app, *kind, width, share, offset)
        };
        remaining = remaining.saturating_sub(lines.len());
        out.extend(lines);
        hits.extend(rows);
        offset += match kind {
            Kind::Open => app.github.rows().len(),
            Kind::Shipped => app.shipped.authors(app.observed_at).len(),
            Kind::Build => app.builds.runs().len(),
        };
    }
    (out, hits)
}
fn observed_lines(
    app: &App,
    kind: crate::github::Kind,
    width: usize,
    height: usize,
    offset: usize,
) -> (Vec<Line<'static>>, Vec<Option<usize>>) {
    use crate::github::Kind;
    let t = &app.theme;
    let d = kind.dashboard(app);
    let groups = d.authors(app.observed_at);
    let runs = d.runs();
    let observed = if kind == Kind::Shipped {
        groups.values().map(Vec::len).sum()
    } else {
        runs.iter().filter(|r| r.known()).count()
    };
    let summary = if d.sources.iter().all(|s| s.at.is_none()) {
        "Counts unavailable".into()
    } else if kind == Kind::Shipped {
        format!("{observed} observed merged / rolling 24h")
    } else {
        format!(
            "{observed} active observed · {} unknown",
            runs.iter().filter(|r| !r.known()).count()
        )
    };
    let mut out = vec![
        ui::rule(
            t,
            if kind == Kind::Shipped {
                "SHIPPED"
            } else {
                "BUILD QUEUE"
            },
            "observed",
            t.sub,
            width,
        ),
        Line::from(sp(summary, t.sub)),
    ];
    let mut hits = vec![None, None];
    for s in &d.sources {
        out.push(Line::from(sp(
            clip(&s.status(app.observed_tick), width),
            if s.error.is_some() { t.check } else { t.text },
        )));
        hits.push(None);
        if kind == Kind::Shipped && s.total == 100 {
            out.push(Line::from(sp(" first 100; more may exist", t.check)));
            hits.push(None);
        }
    }
    if kind == Kind::Build {
        out.push(Line::from(sp(
            "latest100-run sample · older may be missing",
            t.sub,
        )));
        hits.push(None);
    }
    let labels: Vec<_> = if kind == Kind::Shipped {
        groups
            .iter()
            .map(|(a, rows)| {
                (
                    format!("@{a} · {} PRs observed", rows.len()),
                    String::new(),
                    d.sources.iter().any(|s| s.error.is_some()),
                )
            })
            .collect()
    } else {
        runs.iter()
            .map(|r| {
                (
                    format!("{} · {}", r.label(), r.workflow),
                    format!(
                        "  {} · {} · {}",
                        r.branch,
                        r.created.filter(|at| *at <= app.observed_at).map_or_else(
                            || "age unknown".into(),
                            |at| age((app.observed_at - at).saturating_mul(1000))
                        ),
                        r.repo
                    ),
                    d.sources
                        .iter()
                        .any(|s| s.repo == r.repo && s.error.is_some()),
                )
            })
            .collect()
    };
    if labels.is_empty() && d.sources.iter().any(|s| s.at.is_some()) {
        out.push(Line::from(sp(
            if kind == Kind::Shipped {
                "None observed in cached 24h sample"
            } else {
                "None active observed in latest100 sample"
            },
            t.sub,
        )));
        hits.push(None);
    }
    let per_row = if kind == Kind::Shipped { 1 } else { 2 };
    let remaining = height.saturating_sub(out.len());
    let mut fit = (remaining / per_row).min(labels.len());
    if fit < labels.len() {
        fit = remaining.saturating_sub(1) / per_row;
    }
    let cursor = app.row.checked_sub(offset).filter(|i| *i < labels.len());
    let start = cursor
        .map_or(0, |i| (i + 1).saturating_sub(fit))
        .min(labels.len().saturating_sub(fit));
    for (i, (label, meta, stale)) in labels.iter().enumerate().skip(start).take(fit) {
        let label = format!("{label}{}", if *stale { " · stale/partial" } else { "" });
        out.push(ui::row(
            t,
            vec![sp(clip(&label, width.saturating_sub(1)), t.sub)],
            app.row == offset + i,
            false,
            t.sub,
            width,
        ));
        hits.push(Some(offset + i));
        if per_row == 2 {
            out.push(ui::row(
                t,
                vec![sp(clip(meta, width.saturating_sub(1)), t.sub)],
                app.row == offset + i,
                false,
                t.sub,
                width,
            ));
            hits.push(Some(offset + i));
        }
    }
    if fit < labels.len() {
        out.push(Line::from(sp(
            format!(" +{} outside view · j/k to browse", labels.len() - fit),
            t.dim,
        )));
        hits.push(None);
    }
    out.truncate(height);
    hits.truncate(height);
    (out, hits)
}

fn pr_lines(
    app: &App,
    width: usize,
    height: usize,
    offset: usize,
) -> (Vec<Line<'static>>, Vec<Option<usize>>) {
    use crate::github::Ci;
    let t = &app.theme;
    let prs = app.github.rows();
    let counts = [
        Ci::Failing,
        Ci::Running,
        Ci::Unknown,
        Ci::Passing,
        Ci::Draft,
    ]
    .map(|ci| prs.iter().filter(|p| p.ci == ci).count());
    let summary = if app.github.sources.iter().all(|s| s.at.is_none()) {
        "CI counts unavailable".into()
    } else {
        let prefix = if !app.filter.is_empty() {
            "filtered"
        } else if app
            .github
            .sources
            .iter()
            .any(|s| s.at.is_none() || s.error.is_some())
        {
            "cached/partial"
        } else {
            "CI"
        };
        format!(
            "{prefix}: {} fail {} run {} ? {} pass {} draft",
            counts[0], counts[1], counts[2], counts[3], counts[4]
        )
    };
    let mut out = vec![
        ui::rule(t, "OPEN PRS", "observed CI", t.sub, width),
        Line::from(sp(summary, t.sub)),
    ];
    let mut hits = vec![None, None];
    for source in &app.github.sources {
        out.push(Line::from(sp(
            clip(&source.status(app.observed_tick), width),
            if source.error.is_some() {
                t.check
            } else {
                t.text
            },
        )));
        hits.push(None);
        if source.total == 100 {
            out.push(Line::from(sp(" first 100; more may exist", t.check)));
            hits.push(None);
        }
    }
    let remaining = height.saturating_sub(out.len());
    let fit = (remaining / 2).min(prs.len());
    let fit = if fit < prs.len() && remaining % 2 == 0 {
        fit.saturating_sub(1)
    } else {
        fit
    };
    let cursor = app.row.checked_sub(offset).filter(|&i| i < prs.len());
    let start = cursor
        .map_or(0, |i| (i + 1).saturating_sub(fit))
        .min(prs.len().saturating_sub(fit));
    for (i, p) in prs.iter().enumerate().skip(start).take(fit) {
        let color = match p.ci {
            Ci::Failing => t.need,
            Ci::Running => t.check,
            Ci::Passing => t.ok,
            Ci::Unknown | Ci::Draft => t.sub,
        };
        let mark = if app.ascii { "#" } else { "■" };
        let title = format!(
            "{mark} {}{} #{} {}",
            p.ci.word(),
            if p.truncated { " (truncated)" } else { "" },
            p.number,
            p.title
        );
        out.push(ui::row(
            t,
            vec![sp(clip(&title, width.saturating_sub(1)), color)],
            app.row == offset + i,
            false,
            color,
            width,
        ));
        let source = app.github.sources.iter().find(|s| s.repo == p.repo);
        let now = app.observed_at;
        let age = if p.created > 0 {
            age(now.saturating_sub(p.created).saturating_mul(1000))
        } else {
            "age unknown".into()
        };
        let mut metadata = format!("  @{} · {} · {}", p.author, age, p.repo);
        if !p.review.is_empty() {
            metadata += &format!(" · {}", p.review);
        }
        if source.is_some_and(|s| s.error.is_some()) {
            metadata += " · stale";
        }
        if p.truncated {
            metadata += " · checks truncated";
        }
        out.push(ui::row(
            t,
            vec![sp(clip(&metadata, width.saturating_sub(1)), t.sub)],
            app.row == offset + i,
            false,
            color,
            width,
        ));
        hits.extend([Some(offset + i), Some(offset + i)]);
    }
    if fit < prs.len() {
        out.push(Line::from(sp(
            format!(" +{} outside view · j/k to browse", prs.len() - fit),
            t.dim,
        )));
        hits.push(None);
    }
    out.truncate(height);
    hits.truncate(height);
    (out, hits)
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
}

/// Every row the cursor can be on, in cursor order.
pub(crate) fn ids(app: &App) -> Vec<RowId> {
    let g = &app.data.glance;
    let asks = g
        .needs_you
        .iter()
        .filter(|_| app.config.panels[0])
        .map(|a| RowId::Ask(a.ask_id));
    let checks = g
        .attention
        .iter()
        .filter(|_| app.config.panels[1])
        .map(|a| RowId::Check(a.root_id, a.kind.clone()));
    let prs = app
        .github
        .rows()
        .into_iter()
        .filter(|_| app.config.panels[3])
        .map(|p| RowId::Pr(p.repo.clone(), p.number));
    let campaigns = g
        .campaigns
        .iter()
        .filter(|c| !c.parked)
        .chain(g.campaigns.iter().filter(|c| c.parked))
        .filter(|_| app.config.panels[2])
        .map(|c| RowId::Campaign(c.id));
    let authors = app
        .shipped
        .authors(app.observed_at)
        .into_keys()
        .filter(|_| app.config.panels[4])
        .map(RowId::Author);
    let runs = app
        .builds
        .runs()
        .into_iter()
        .filter(|_| app.config.panels[5])
        .map(|r| RowId::Run(r.repo.clone(), r.id));
    asks.chain(checks)
        .chain(prs)
        .chain(authors)
        .chain(runs)
        .chain(campaigns)
        .collect()
}

/// The same visible identities drive Tab, keyboard, mouse and snapshot restoration.
pub(crate) fn sections(app: &App) -> Vec<usize> {
    let mut out = vec![];
    let mut previous = None;
    for (i, row) in ids(app).iter().enumerate() {
        let kind = std::mem::discriminant(row);
        if previous != Some(kind) {
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
    Pr(&'a crate::github::Pr),
    Author(String),
    Run(&'a crate::github::Run),
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
            .github
            .rows()
            .into_iter()
            .find(|p| p.repo == *repo && p.number == *number)
            .map_or(Selected::Nothing, Selected::Pr),
        Some(RowId::Author(author)) => Selected::Author(author.clone()),
        Some(RowId::Run(repo, id)) => app
            .builds
            .runs()
            .into_iter()
            .find(|r| r.repo == *repo && r.id == *id)
            .map_or(Selected::Nothing, Selected::Run),
        None => Selected::Nothing,
    }
}

/// The footer lists only the keys that do something for the selected row.
pub(crate) fn hints(app: &App, wide: bool) -> Vec<(&'static str, &'static str)> {
    let g = &app.data.glance;
    // `⏎` is offered only where there is a pane to go to.
    let go = |host: &str, pane: &str| match (pane.is_empty(), g.local(host), wide) {
        (true, ..) => None,
        (_, false, _) => Some(hint("⏎", Some("how to get there"))),
        (_, true, true) => Some(hint("⏎", Some("go to lead"))),
        (_, true, false) => Some(hint("⏎", None)),
    };
    let mut out: Vec<_> = match selected(app) {
        Selected::Ask(a) => vec![
            go(&a.host, &a.pane_id),
            Some(hint("a", None)),
            Some(hint("l", None)),
        ],
        Selected::Check(a) => vec![
            go(&a.host, &a.pane_id),
            (a.root_id != 0).then(|| hint("l", None)),
        ],
        Selected::Campaign(c) => vec![
            go(&c.host, &c.pane_id),
            Some(hint("l", None)),
            Some(hint("p", None)),
        ],
        Selected::Pr(_) | Selected::Author(_) | Selected::Run(_) => vec![Some(hint("r", None))],
        Selected::Nothing => vec![Some(hint("c", Some("all campaigns")))],
    }
    .into_iter()
    .flatten()
    .collect();
    if app.fetch.error.is_some() {
        out.retain(|(key, _)| *key != "r");
        out.insert(0, hint("r", Some("retry")));
    }
    if app.detail && wide && !matches!(selected(app), Selected::Nothing) {
        out.splice(0..0, [hint("j k", Some("scroll")), hint("h", Some("list"))]);
    }
    if wide {
        out.push(hint("1-6", None));
        out.extend([
            hint("/", None),
            hint("y", None),
            hint("s", None),
            hint("?", None),
            hint("q", None),
        ]);
    } else {
        out.push(hint("?", None));
    }
    if matches!(
        selected(app),
        Selected::Pr(_) | Selected::Author(_) | Selected::Run(_)
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
    let [head, switches, warning, notes, banner, body, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(if wide { 1 } else { 2 }),
        Constraint::Length(u16::from(app.config_warning.is_some())),
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
    let labels = ["asks", "checks", "camps", "PRs", "Shipped", "Build"];
    let toggles = labels
        .iter()
        .enumerate()
        .map(|(i, name)| {
            format!(
                "{} {}{}",
                i + 1,
                name,
                if app.config.panels[i] { "+" } else { "-" }
            )
        })
        .collect::<Vec<_>>()
        .join(" · ");
    let toggle_lines = if wide {
        vec![Line::from(sp(toggles, t.sub))]
    } else {
        let parts: Vec<_> = toggles.split(" · ").collect();
        vec![
            Line::from(sp(parts[..3].join(" · "), t.sub)),
            Line::from(sp(parts[3..].join(" · "), t.sub)),
        ]
    };
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
    ui::footer(f, app, foot, &hints(app, wide));

    if !app.config.panels.iter().any(|p| *p) {
        f.render_widget(
            Paragraph::new("All panels hidden · press 1-6 to show").centered(),
            body,
        );
        return;
    }
    let mut list = body;
    if wide {
        // No inset here: the list has its own gutter column.
        let detail = app.detail && !matches!(selected(app), Selected::Nothing);
        let block = ui::panel(t, "Glance", !detail);
        list = block.inner(Rect { width: 60, ..body });
        f.render_widget(block, Rect { width: 60, ..body });
    }
    let mut compact = list.height < 8;
    let ledger_state = g.needs_you.is_empty()
        && g.attention.is_empty()
        && g.campaigns.is_empty()
        && g.quiet.count == 0;
    let state_height = if ledger_state && (!compact || ids(app).is_empty()) {
        (list.height as usize).saturating_sub(external_height(app, list.height as usize))
    } else {
        0
    };
    if ledger_state && state_height > 0 {
        state(
            f,
            app,
            Rect {
                height: state_height as u16,
                ..list
            },
        );
    }
    let list_rows = Rect {
        y: list.y + state_height as u16,
        height: list.height.saturating_sub(state_height as u16),
        ..list
    };
    let (mut lines, mut hits) = if ledger_state
        && [&app.github, &app.shipped, &app.builds]
            .iter()
            .any(|d| !d.sources.is_empty())
    {
        // The placeholder reserve was already removed from list_rows.
        external_lines(app, list_rows.width as usize, list_rows.height as usize, 0)
    } else if compact {
        compact_layout(app, list_rows.width as usize, list_rows.height as usize)
    } else {
        layout(app, list_rows.width as usize, list_rows.height as usize)
    };
    // Rules, warnings and independent source rows can exhaust a section's share.
    // Fall back using the same identity when the expanded layout cannot draw it.
    if !ids(app).is_empty() && !hits.contains(&Some(app.row)) {
        compact = true;
        (lines, hits) = compact_layout(app, list_rows.width as usize, list_rows.height as usize);
    }
    // Folded campaigns are out of the cursor's reach; the compact list scrolls every identity.
    let p = plan(
        g,
        (list.height as usize).saturating_sub(external_height(app, list.height as usize)),
        if app.row >= g.needs_you.len() + g.attention.len() {
            app.row.saturating_sub(external_count(app))
        } else {
            app.row
        },
    );
    let parked = g.campaigns.iter().filter(|c| c.parked).count();
    app.seen.borrow_mut().rows = if compact {
        ids(app).len()
    } else {
        g.needs_you.len()
            + g.attention.len()
            + external_count(app)
            + p.shown
            + if p.tail == Tail::Rows { parked } else { 0 }
    };
    for (y, row) in hits.iter().enumerate().take(list_rows.height as usize) {
        if let Some(row) = row {
            let line = Rect {
                y: list_rows.y + y as u16,
                height: 1,
                ..list
            };
            app.hit(line, Hit::Row(*row));
        }
    }
    f.render_widget(Paragraph::new(lines), list_rows);
    if let Some(error) = stale {
        // The last frame stays, dimmed, and says so.

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
            lines.push(Line::from(sp("No ledger snapshot yet.", t.sub)));
            lines.push(Line::raw(""));
            lines.push(Line::from(sp(
                format!("try {} · next in {}s", fetch.tries, fetch.retry_in_s),
                t.dim,
            )));
            lines
        }
        _ if (!app.config.panels[..3].iter().all(|p| *p) || !app.filter.is_empty())
            && g.needs_you.is_empty()
            && g.attention.is_empty()
            && g.campaigns.is_empty() =>
        {
            vec![
                Line::from(sp("No visible ledger rows", t.sub)),
                Line::from(sp("1-3 show panels · / filters", t.dim)),
            ]
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
            let mut head = vec![
                sp("lead ", t.dim),
                ui::lead_mark(t, &c.lead, c.lead_waiting),
                sp(format!(" {}   lanes ", c.lead), t.sub),
            ];
            head.extend(ui::pips(t, &c.lanes, 7));
            if let Some(label) = c.tmp_label() {
                head.push(sp(format!(" · {label}"), t.dim));
            }
            (
                format!("{} · #{}", c.name, c.id),
                vec![Line::from(head)],
                c.status_text().to_string(),
            )
        }
        Selected::Pr(p) => (
            format!("{} #{}", p.repo, p.number),
            vec![Line::from(sp(
                format!(
                    "{} · @{}{}",
                    p.ci.word(),
                    p.author,
                    if p.review.is_empty() {
                        String::new()
                    } else {
                        format!(" · {}", p.review)
                    }
                ),
                t.sub,
            ))],
            format!(
                "{}\n{}\nObserved checks only; not merge readiness.{}",
                p.title,
                p.repo,
                if p.truncated {
                    " Check list truncated; completeness unknown."
                } else {
                    ""
                }
            ),
        ),
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
        Selected::Nothing => return,
    };
    // The pane's inside, as `ui::pane` will draw it, so the text wraps before the title
    // says how much of it there is.
    let (width, h) = (
        area.width.saturating_sub(4) as usize,
        area.height.saturating_sub(2) as usize,
    );
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
        lines.push(Line::from(ui::fit(line, width)));
        lines.extend(
            wrap(&o.description, width.saturating_sub(4))
                .into_iter()
                .filter(|l| !l.is_empty())
                .map(|l| Line::from(sp(format!("    {l}"), t.dim))),
        );
    }
    // Longer than its box, it scrolls when focused and says where it is, as the campaign
    // view's panes do.
    let total = lines.len();
    app.seen.borrow_mut().page = (h, total);
    app.hit(area, Hit::Detail);
    let first = app.scroll.min(total.saturating_sub(h));
    let title = if total > h && h > 0 {
        format!("{title} · {}-{}/{total}", first + 1, first + h)
    } else {
        title
    };
    let inner = ui::pane(f, t, area, &title, app.detail);
    f.render_widget(
        Paragraph::new(lines.into_iter().skip(first).take(h).collect::<Vec<_>>()),
        inner,
    );
}

/// The lower right pane: the selected row's campaign over the last four hours.
fn activity(f: &mut Frame, app: &App, area: Rect) {
    let (t, g) = (&app.theme, &app.data.glance);
    let root = match selected(app) {
        Selected::Ask(a) => a.root_id,
        Selected::Check(a) => a.root_id,
        Selected::Campaign(c) => c.id,
        Selected::Pr(_) | Selected::Author(_) | Selected::Run(_) | Selected::Nothing => return,
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
