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

/// Wait reasons that mean memory or pressure: amber.
fn pressing(reason: &str) -> bool {
    matches!(reason, "memory_budget" | "psi" | "recovery")
}

/// The machine's line: memory, pressure and the last stop. slotr reports them once, for
/// every pool.
fn machine(app: &App, w: usize) -> Vec<Line<'static>> {
    let (t, s) = (&app.theme, &app.data.slotr);
    let st = &s.stats;
    let mut lines = vec![Line::from(ui::fit(
        vec![
            sp(" memory ", t.dim),
            sp(format!("{} free", mem(st.available_mib)), t.text),
            sp(format!(" of {}", mem(st.total_mib)), t.dim),
            sp(" · psi ", t.dim),
            sp(
                format!("{:.1} / {:.1}", st.psi_full_avg10, st.psi_full_avg60),
                if st.psi_full_avg10 >= 1.0 {
                    t.check
                } else {
                    t.text
                },
            ),
            sp(format!(" · load {:.1}/{}", st.load1, st.cores), t.dim),
        ],
        w,
    ))];
    let stop = match &s.last_stop {
        Some(stop) if !stop.at.is_empty() => {
            let when = ago(app, Some(stop.at.as_str()))
                .map_or(String::new(), |a| format!("{} ago", age(a)));
            vec![
                sp(" last stop ", t.dim),
                sp(when, t.text),
                sp(format!(" · {} · {}", stop.reason, stop.run), t.sub),
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

/// The pool's rule and budget line.
fn head(app: &App, name: &str, p: &Pool, w: usize) -> Vec<Line<'static>> {
    let t = &app.theme;
    let count = format!(
        "{}/{} slots · {} queued",
        p.holders.len(),
        p.slots,
        p.queue.len()
    );
    let b = &p.budget;
    // projected_free already takes off one more run of the pool's default cost: below
    // zero it means "would not fit", not a fault.
    let room = if b.projected_free_mib >= 0.0 {
        sp(
            format!(" room for one more · {} spare", mem(b.projected_free_mib)),
            t.ok,
        )
    } else {
        sp(
            format!(
                " no room for one more · {} short",
                mem(-b.projected_free_mib)
            ),
            t.check,
        )
    };
    vec![
        ui::rule(t, &name.to_uppercase(), &count, t.sub, w),
        Line::from(ui::fit(
            vec![
                room,
                sp(
                    format!(
                        " · reserve {} · outstanding {}",
                        mem(b.reserve_mib),
                        mem(b.outstanding_mib)
                    ),
                    t.dim,
                ),
            ],
            w,
        )),
    ]
}

/// A holder: two lines, the run and its figures, then where it runs.
fn holder(app: &App, r: &SlotRow, wide: bool, w: usize) -> [Vec<Span<'static>>; 2] {
    let t = &app.theme;
    let color = if r.stopping() {
        t.need
    } else if r.warned_at.is_some() {
        t.check
    } else {
        t.work
    };
    let mut left = vec![
        if r.priority() {
            bold("◆", t.accent)
        } else {
            sp("●", color)
        },
        sp(format!(" {}", r.campaign), t.text),
        sp(format!(" {}", r.purpose), t.sub),
    ];
    if let Some(kind) = r.kind.as_deref().filter(|k| !k.is_empty()) {
        left.insert(1, sp(format!(" {kind}"), t.accent));
    }
    let held = ago(app, r.admitted_at.as_deref().or(r.since.as_deref()));
    let lease = ago(app, r.lease_expires_at.as_deref()).map(|a| -a);
    let state = if r.stopping() {
        sp("stopping ", t.need)
    } else if r.warned_at.is_some() {
        sp("warned ", t.check)
    } else {
        Span::raw("")
    };
    let mut right = vec![
        state,
        sp(format!("held {}", held.map_or("?".into(), age)), t.sub),
    ];
    if let Some(lease) = lease {
        let rest = if lease > 0 { age(lease) } else { "over".into() };
        right.push(sp(format!(" · lease {rest}"), t.dim));
    }
    let cost = match r.anon_mib {
        Some(anon) => format!(" · {}/{}", mem(anon), mem(r.cost_mib)),
        None => format!(" · {}", mem(r.cost_mib)),
    };
    right.push(sp(cost, t.dim));
    let mut place = vec![sp("   ↳ ", t.dim)];
    let mut parts = vec![];
    if !r.task.is_empty() {
        parts.push(format!("task {}", r.task));
    }
    if !r.pane.is_empty() {
        parts.push(format!("pane {}", r.pane));
    }
    if parts.is_empty() {
        parts.push(if r.notify.is_empty() {
            "no task or pane".into()
        } else {
            format!("notify {}", r.notify)
        });
    }
    place.push(sp(parts.join(" · "), t.dim));
    if wide {
        place.push(sp(format!("   {}", r.run), t.dim));
        if r.root_id != 0 && r.root_name != r.campaign {
            place.push(sp(format!(" · campaign {}", r.root_name), t.dim));
        }
    }
    [ui::spread(left, right, w), place]
}

/// A waiter: one line, in queue order.
fn waiter(app: &App, r: &SlotRow, w: usize) -> Vec<Span<'static>> {
    let t = &app.theme;
    let mut left = vec![
        if r.priority() {
            bold("◆", t.accent)
        } else {
            sp("○", t.dim)
        },
        sp(format!(" {:>2}", r.position), t.dim),
        sp(format!(" {}", r.campaign), t.text),
        sp(format!(" {}", r.purpose), t.sub),
    ];
    if let Some(kind) = r.kind.as_deref().filter(|k| !k.is_empty()) {
        left.insert(2, sp(format!(" {kind}"), t.accent));
    }
    let reason = if r.wait_reason.is_empty() {
        "waiting"
    } else {
        &r.wait_reason
    };
    let waited = ago(app, r.since.as_deref()).map_or("?".into(), age);
    let right = vec![
        sp(
            format!("{reason} "),
            if pressing(reason) { t.check } else { t.sub },
        ),
        sp(format!("{waited} · {}", mem(r.cost_mib)), t.dim),
    ];
    ui::spread(left, right, w)
}

pub(crate) fn draw(f: &mut Frame, app: &App) {
    let t = &app.theme;
    let area = f.area();
    let wide = area.width >= 100;
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
        app.slotr.in_flight || !app.slotr.loaded,
        app.slotr.tick,
        None,
    );
    ui::header(f, &shown, head_area, vec![sp("slotr", t.dim)]);
    ui::footer(
        f,
        &shown,
        foot,
        &[
            hint("⏎", None),
            hint("l", None),
            hint("y", None),
            hint("r", None),
            hint("s", Some("back")),
            hint("?", None),
        ],
    );
    let w = body.width as usize;
    let s = &app.data.slotr;
    let mut top = vec![];
    if let Some(error) = &app.slotr.error {
        top.push(Line::from(ui::fit(
            vec![sp(format!(" ! slotr unavailable: {error}"), t.check)],
            w,
        )));
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
            let [first, second] = holder(app, r, wide, w - 1);
            lines.push(ui::row(t, first, i == app.row, false, t.accent, w));
            lines.push(ui::row(t, second, i == app.row, false, t.accent, w));
            owner.extend([Some(i), Some(i)]);
            i += 1;
        }
        if !pool.queue.is_empty() {
            lines.push(Line::from(sp("  queue", t.sub)));
            owner.push(None);
        }
        for r in &pool.queue {
            lines.push(ui::row(
                t,
                waiter(app, r, w - 1),
                i == app.row,
                false,
                t.accent,
                w,
            ));
            owner.push(Some(i));
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
