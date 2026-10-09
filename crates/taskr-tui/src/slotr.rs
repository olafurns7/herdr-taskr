//! The slotr view (`s`): one block per pool, its budget, its holders and its queue in
//! order, from `taskr slotr --json`. Ages are taken against the hub's clock when it read
//! slotr, plus the read's own age, so a viewer's clock skew does not show.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::{Line, Span},
    widgets::Paragraph,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use unicode_width::UnicodeWidthStr;

use crate::{
    App, Hit,
    keys::hint,
    model::{Pool, SlotRow, Slotr},
    ui::{self, age, bold, sp},
};

/// The rows the cursor walks: per pool in name order, its holders, then its queue.
pub(crate) fn rows(s: &Slotr) -> Vec<&SlotRow> {
    s.pools
        .values()
        .flat_map(|p| p.holders.iter().chain(&p.queue))
        .collect()
}

fn ms(at: &str) -> Option<i64> {
    let t = OffsetDateTime::parse(at, &Rfc3339).ok()?;
    Some((t.unix_timestamp_nanos() / 1_000_000) as i64)
}

/// How long ago `at` was, on screen now.
fn ago(app: &App, at: Option<&str>) -> Option<i64> {
    Some(ms(&app.data.slotr.now)? - ms(at?)? + app.slotr.age_ms)
}

/// Memory as the owner reads it: `512M`, `3.2G`.
fn mem(mib: f64) -> String {
    if mib.abs() < 1024.0 {
        format!("{mib:.0}M")
    } else {
        format!("{:.1}G", mib / 1024.0)
    }
}

/// A figure slotr could not read is `?`.
fn or_unknown(v: Option<f64>, show: impl Fn(f64) -> String) -> String {
    v.map_or("?".into(), show)
}

/// Wait reasons that mean memory or pressure: amber.
fn pressing(reason: &str) -> bool {
    matches!(reason, "memory_budget" | "psi" | "load" | "recovery")
}

/// Whether any run waits for `reason`: the machine line lights up only then.
fn waits(s: &Slotr, reason: &str) -> bool {
    s.pools
        .values()
        .any(|p| p.queue.iter().any(|r| r.wait_reason == reason))
}

/// The machine's lines: memory, pressure and the last stop. slotr reports them once, for
/// every pool; the reserve and what holders may still take are the same in every budget.
fn machine(app: &App, w: usize) -> Vec<Line<'static>> {
    let (t, s) = (&app.theme, &app.data.slotr);
    let st = &s.stats;
    let lit = |reason| if waits(s, reason) { t.check } else { t.text };
    let mut memory = vec![
        sp(" memory ", t.dim),
        sp(
            format!("{} free", or_unknown(st.available_mib, mem)),
            lit("memory_budget"),
        ),
        sp(format!(" of {}", or_unknown(st.total_mib, mem)), t.dim),
    ];
    if let Some(p) = s.pools.values().next() {
        memory.push(sp(
            format!(
                " · reserve {} · holders may still take {}",
                mem(p.budget.reserve_mib),
                mem(p.budget.outstanding_mib)
            ),
            t.dim,
        ));
    }
    let one = |v: f64| format!("{v:.1}");
    let pressure = vec![
        sp(" psi ", t.dim),
        sp(
            format!(
                "10s {} · 60s {}",
                or_unknown(st.psi_full_avg10, one),
                or_unknown(st.psi_full_avg60, one)
            ),
            lit("psi"),
        ),
        sp(
            format!(
                " · load {} on {} cores",
                or_unknown(st.load1, one),
                or_unknown(st.cores, |c| format!("{c:.0}"))
            ),
            t.dim,
        ),
    ];
    let mut lines = vec![
        Line::from(ui::fit(memory, w)),
        Line::from(ui::fit(pressure, w)),
    ];
    let stop = match &s.last_stop {
        Some(stop) if !stop.at.is_empty() => {
            let when = ago(app, Some(stop.at.as_str()))
                .map_or(String::new(), |a| format!("{} ago", age(a)));
            vec![
                sp(" last stop ", t.dim),
                sp(when, t.text),
                sp(format!(" · {} · {}", stop.run, stop.reason), t.sub),
            ]
        }
        _ => vec![sp(" no stop yet", t.dim)],
    };
    lines.push(Line::from(ui::fit(stop, w)));
    if s.schema_version != 1 {
        lines.push(Line::from(sp(
            format!(
                " slotr schema {}; some fields may be missing",
                s.schema_version
            ),
            t.dim,
        )));
    }
    lines
}

/// The pool's rule and whether one more run of its default cost would be admitted now.
fn head(app: &App, name: &str, p: &Pool, w: usize) -> Vec<Line<'static>> {
    let t = &app.theme;
    let count = format!(
        "{}/{} slots · {} queued",
        p.holders.len(),
        p.slots,
        p.queue.len()
    );
    let mut lines = vec![ui::rule(t, &name.to_uppercase(), &count, t.sub, w)];
    let b = &p.budget;
    // slotr admits a run when what is left after it still covers the reserve.
    if let Some(free) = b.projected_free_mib {
        let spare = free - b.reserve_mib;
        let room = if spare >= 0.0 {
            sp(format!(" one more fits · {} spare", mem(spare)), t.ok)
        } else {
            sp(
                format!(" one more would not fit · {} short", mem(-spare)),
                t.check,
            )
        };
        lines.push(Line::from(ui::fit(vec![room], w)));
    }
    lines
}

/// The row's mark: `▲` for a priority run, else `mark`.
fn glyph(t: &crate::theme::Theme, r: &SlotRow, mark: Span<'static>) -> Span<'static> {
    if r.priority() {
        bold("▲", t.accent)
    } else {
        mark
    }
}

/// Campaign padded to the view's column, then purpose, then H1's kind.
fn name(app: &App, r: &SlotRow, cw: usize) -> Vec<Span<'static>> {
    let t = &app.theme;
    let mut out = vec![
        sp(format!(" {}", ui::pad(&r.campaign, cw)), t.text),
        sp(format!(" {}", r.purpose), t.sub),
    ];
    if let Some(kind) = r.kind.as_deref().filter(|k| !k.is_empty()) {
        out.push(sp(format!(" · {kind}"), t.dim));
    }
    out
}

/// What slotr is doing to a holder, if anything but letting it run.
fn state(r: &SlotRow) -> &str {
    if r.stopping() {
        "stopping"
    } else if !matches!(r.state.as_str(), "" | "running") {
        &r.state
    } else if r.warned_at.is_some() {
        "warned"
    } else {
        ""
    }
}

/// The figures on the right, or on their own line under the name below 70 columns.
fn lay(
    left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
    indent: &str,
    narrow: bool,
    w: usize,
) -> Vec<Vec<Span<'static>>> {
    if narrow {
        let mut under = vec![Span::raw(indent.to_string())];
        under.extend(right);
        vec![left, under]
    } else {
        vec![ui::spread(left, right, w)]
    }
}

/// A holder: the run and its figures, then where it runs.
fn holder(
    app: &App,
    r: &SlotRow,
    cw: usize,
    wide: bool,
    narrow: bool,
    w: usize,
) -> Vec<Vec<Span<'static>>> {
    let t = &app.theme;
    let state = state(r);
    let mut left = vec![glyph(
        t,
        r,
        sp("●", if state.is_empty() { t.work } else { t.check }),
    )];
    left.extend(name(app, r, cw));
    let mut right = match state {
        "" => vec![],
        "stopping" => vec![bold("stopping", t.check), sp(" · ", t.dim)],
        _ => vec![sp(state.to_string(), t.check), sp(" · ", t.dim)],
    };
    let held = ago(app, r.admitted_at.as_deref().or(r.since.as_deref()));
    right.push(sp(format!("held {}", held.map_or("?".into(), age)), t.sub));
    match ago(app, r.lease_expires_at.as_deref()).map(|a| -a) {
        Some(lease) if lease > 0 => right.push(sp(format!(" · {} left", age(lease)), t.dim)),
        Some(_) if state != "overdue" => right.push(sp(" · lease over", t.dim)),
        _ => {}
    }
    let cost = match r.anon_mib {
        Some(anon) => format!(" · {}/{}", mem(anon), mem(r.cost_mib)),
        None => format!(" · {}", mem(r.cost_mib)),
    };
    right.push(sp(cost, t.dim));
    let mut parts = vec![];
    if !r.task.is_empty() {
        parts.push(format!("task {}", r.task));
    }
    if !r.pane.is_empty() {
        parts.push(format!("pane {}", r.pane));
    }
    if parts.is_empty() {
        parts.push("no task or pane".into());
    }
    if !r.run.is_empty() {
        parts.push(format!("run {}", r.run));
    }
    if wide && r.root_id != 0 && r.root_name != r.campaign {
        parts.push(format!("campaign {}", r.root_name));
    }
    let mut lines = lay(left, right, "   ", narrow, w);
    lines.push(vec![sp("   ↳ ", t.dim), sp(parts.join(" · "), t.dim)]);
    lines
}

/// A waiter, in queue order: why it waits, for how long, and its cost.
fn waiter(app: &App, r: &SlotRow, cw: usize, narrow: bool, w: usize) -> Vec<Vec<Span<'static>>> {
    let t = &app.theme;
    let mut left = vec![
        glyph(t, r, sp("○", t.dim)),
        sp(format!(" {:>2}", r.position), t.dim),
    ];
    left.extend(name(app, r, cw));
    let reason = if r.wait_reason.is_empty() {
        "waiting"
    } else {
        &r.wait_reason
    };
    let color = match reason {
        "ready" => t.ok,
        r if pressing(r) => t.check,
        _ => t.sub,
    };
    let mut right = vec![sp(reason.to_string(), color)];
    if let Some(pid) = r.legacy_holder_pid.filter(|_| reason == "legacy_lock") {
        right.push(sp(format!(" pid {pid}"), t.sub));
    }
    let waited = ago(app, r.since.as_deref()).map_or("?".into(), age);
    right.push(sp(format!(" for {waited} · {}", mem(r.cost_mib)), t.dim));
    lay(left, right, "      ", narrow, w)
}

/// The reason slotr cannot be read, as the owner can act on it.
fn trouble(error: &str) -> &str {
    if error.contains("unknown command") {
        "taskr here or on the hub is older than taskr slotr: update both"
    } else {
        error
    }
}

pub(crate) fn draw(f: &mut Frame, app: &App) {
    let t = &app.theme;
    let area = f.area();
    let (wide, narrow) = (area.width >= 100, area.width < 70);
    let [head_area, body, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);
    // The header's age and spinner are this view's read; the pill stays the glance's.
    let mut shown = app.clone();
    let fetch = &mut shown.fetch;
    (fetch.age_ms, fetch.in_flight, fetch.tick, fetch.live) = (
        app.slotr.age_ms,
        app.slotr.in_flight || (!app.slotr.loaded && app.slotr.error.is_none()),
        app.slotr.tick,
        None,
    );
    ui::header(f, &shown, head_area, vec![sp("slotr", t.dim)]);
    let hints = if !app.slotr.loaded {
        // Nothing to select yet.
        vec![hint("r", None), hint("s", Some("back")), hint("?", None)]
    } else if narrow {
        vec![
            hint("⏎", None),
            hint("l", None),
            hint("s", Some("back")),
            hint("?", None),
        ]
    } else {
        vec![
            hint("⏎", None),
            hint("l", None),
            hint("y", None),
            hint("r", None),
            hint("s", Some("back")),
            hint("?", None),
        ]
    };
    ui::footer(f, &shown, foot, &hints);
    let w = body.width as usize;
    let s = &app.data.slotr;
    let mut top = vec![];
    if let Some(error) = &app.slotr.error {
        for (n, l) in ui::wrap_max(trouble(error), w.saturating_sub(3), 2)
            .into_iter()
            .enumerate()
        {
            top.push(Line::from(sp(
                format!("{}{l}", if n == 0 { " ! " } else { "   " }),
                t.check,
            )));
        }
    }
    if !app.slotr.loaded {
        if app.slotr.error.is_none() {
            top.push(Line::from(sp(" reading slotr", t.dim)));
        }
        f.render_widget(Paragraph::new(top), body);
        return;
    }
    let mut lines = machine(app, w);
    // Each line, with the row it belongs to.
    let mut owner: Vec<Option<usize>> = vec![None; lines.len()];
    let mut i = 0;
    let cw = rows(s)
        .iter()
        .map(|r| r.campaign.width())
        .max()
        .unwrap_or(0)
        .min(16);
    for (name, pool) in &s.pools {
        lines.push(Line::raw(""));
        owner.push(None);
        for l in head(app, name, pool, w) {
            lines.push(l);
            owner.push(None);
        }
        if pool.holders.is_empty() {
            lines.push(Line::from(sp("   no holders", t.dim)));
            owner.push(None);
        }
        for r in &pool.holders {
            for l in holder(app, r, cw, wide, narrow, w - 1) {
                lines.push(ui::row(t, l, i == app.row, false, t.accent, w));
                owner.push(Some(i));
            }
            i += 1;
        }
        if !pool.queue.is_empty() {
            lines.push(Line::from(sp("  queue", t.sub)));
            owner.push(None);
        }
        for r in &pool.queue {
            for l in waiter(app, r, cw, narrow, w - 1) {
                lines.push(ui::row(t, l, i == app.row, false, t.accent, w));
                owner.push(Some(i));
            }
            i += 1;
        }
    }
    if s.pools.is_empty() {
        lines.push(Line::from(sp(" no pools", t.dim)));
        owner.push(None);
    }
    app.seen.borrow_mut().rows = i;
    // A window over the lines that keeps the cursor's row in sight.
    let room = (body.height as usize).saturating_sub(top.len());
    let last = owner.iter().rposition(|o| *o == Some(app.row)).unwrap_or(0);
    let first = (last + 1)
        .saturating_sub(room)
        .min(lines.len().saturating_sub(room));
    let y0 = body.y + top.len() as u16;
    for (n, o) in owner.iter().enumerate().skip(first).take(room) {
        if let Some(row) = o {
            let line = Rect {
                y: y0 + (n - first) as u16,
                height: 1,
                ..body
            };
            app.hit(line, Hit::Row(*row));
        }
    }
    let list = Rect {
        y: y0,
        height: room as u16,
        ..body
    };
    f.render_widget(Paragraph::new(top), body);
    f.render_widget(
        Paragraph::new(lines.into_iter().skip(first).take(room).collect::<Vec<_>>()),
        list,
    );
    // A failed read leaves the last good pools on screen, dimmed.
    if app.slotr.error.is_some() {
        ui::dim(f, list, t.dim);
    }
}
