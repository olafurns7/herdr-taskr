//! Every screen drawn from the fixture, as the golden frames the tests compare and the
//! owner reviews (`cargo run -p taskr-tui --example frames -- frames`).

use ratatui::{
    Terminal,
    backend::TestBackend,
    buffer::Buffer,
    style::{Color, Modifier},
};

use crate::{App, Fetch, Screen, draw, model::Data, theme};

pub struct Spec {
    pub name: String,
    pub width: u16,
    pub height: u16,
    pub light: bool,
    setup: fn(&mut App),
}

/// The synthetic fixture `tools/mkfixture.py` writes: invented names, texts, ids and hosts
/// in the shape of the ledger reads. Nothing in it comes from a ledger.
pub fn fixture() -> Data {
    serde_json::from_str(include_str!("../fixture.json")).expect("the fixture decodes")
}

/// The same data with the text live ledgers hold too: emoji with VS16, a ZWJ sequence and
/// CJK (`tools/mkfixture.py --live`). Still invented.
pub fn live_fixture() -> Data {
    serde_json::from_str(include_str!("../fixture-live.json")).expect("the live fixture decodes")
}

/// The fixture's 12-step ask, longer than a Detail screen. It is kept out of the
/// glance so the other frames do not change.
pub fn long_ask() -> crate::model::Need {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../fixture.json")).expect("the fixture decodes");
    serde_json::from_value(fixture["long_ask"].clone()).expect("the long ask decodes")
}

/// The fixture's structured asks (Q1's `question` and `dialog`): the hub's own dialog ask,
/// a single-select with a recommended option, and a multi-select. Kept out of the glance
/// too; `with_structured` and `--demo` put them in front of the other asks.
pub fn structured_asks() -> Vec<crate::model::Need> {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../fixture.json")).expect("the fixture decodes");
    serde_json::from_value(fixture["structured_asks"].clone()).expect("the structured asks decode")
}

/// The structured asks in front of the fixture's own.
pub fn with_structured(app: &mut App) {
    let mut asks = structured_asks();
    asks.append(&mut app.data.glance.needs_you);
    app.data.glance.needs_you = asks;
}

fn structured(app: &mut App) {
    // The single-select selected.
    with_structured(app);
    app.row = 1;
}

fn answer_single(app: &mut App) {
    with_structured(app);
    app.row = 1;
    app.answer(app.data.glance.needs_you[1].clone());
}

fn answer_multi(app: &mut App) {
    // German and Japanese picked, the cursor on Japanese.
    with_structured(app);
    app.row = 2;
    app.answer(app.data.glance.needs_you[2].clone());
    for choice in [1, 2] {
        app.choose(choice);
        app.toggle();
    }
}

/// The long ask opened with `I`.
fn detail(app: &mut App) {
    app.data.glance.needs_you.push(long_ask());
    app.row = 2;
    app.inspect();
}

/// The same scrolled to its end (`G`; the draw stops at the last line).
fn detail_end(app: &mut App) {
    detail(app);
    app.scroll = usize::MAX;
}

/// `I` on the first row of a kind, on the Dashboard as `setup` leaves it.
fn inspect(app: &mut App, kind: fn(&crate::glance::RowId) -> bool) {
    app.row = crate::glance::ids(&app.view())
        .iter()
        .position(kind)
        .expect("a row of that kind");
    app.inspect();
}

fn asks(app: &mut App) {
    glance(app);
    with_structured(app);
    app.screen = Screen::Asks;
    app.row = 1;
}

fn fleet(app: &mut App) {
    glance(app);
    app.screen = Screen::Fleet;
    app.row = 2;
}

/// Fleet after `l` read tui-frames 40 s ago: its lanes cached under the roster, the cursor
/// on another campaign.
fn fleet_read(app: &mut App) {
    fleet(app);
    app.scope.read_at = Some(app.observed_at - 40);
}

/// The cached campaign under a long name, the cursor on its row: the root and the read age
/// stay whole, the name gives way.
fn fleet_read_long(app: &mut App) {
    fleet_read(app);
    let long = "warehouse-inventory-reconciliation-tui";
    app.data.campaign.root.name = long.into();
    if let Some(c) = app.data.glance.campaigns.iter_mut().find(|c| c.id == 4120) {
        c.name = long.into();
    }
    app.row = 6;
}

/// `a` on a Fleet row: the picker over Fleet, the second ask highlighted.
fn picker(app: &mut App) {
    fleet(app);
    with_structured(app);
    app.picker();
    app.pick_by(1);
}

/// The answer dialog after a snapshot no longer lists its ask.
/// Every outcome an answer sent from here can have, one ask each; the last one's ask has
/// left the snapshot.
fn answered(app: &mut App) {
    use crate::{Outcome, Sent, actions::Job};
    asks(app);
    let asks = app.data.glance.needs_you.clone();
    let mut left = asks[0].clone();
    (left.ask_id, left.campaign) = (69901, "search-index".into());
    let states = [
        Outcome::Recorded {
            answer_id: 61902,
            note: "not delivered to its pane: no pane".into(),
        },
        Outcome::Refused("ask is already answered by event 61850".into()),
        Outcome::Unknown {
            after: std::time::Instant::now(),
        },
        Outcome::Open,
        Outcome::Sending,
        Outcome::Gone,
    ];
    for (i, state) in states.into_iter().enumerate() {
        let ask = match state {
            Outcome::Gone => left.clone(),
            _ => asks[i % asks.len()].clone(),
        };
        app.outcomes.push(Sent {
            job: Job::Answer {
                ask_id: ask.ask_id,
                text: "A: yes".into(),
                prompt: false,
                key: format!("tui-frame-000{i}"),
            },
            ask,
            state,
        });
    }
}

/// A refused answer whose ask has left the snapshot: kept, its text long.
fn kept(app: &mut App) {
    use crate::{Outcome, actions::Job};
    answered(app);
    app.screen = Screen::Glance;
    let sent = app
        .outcomes
        .iter_mut()
        .find(|s| matches!(s.state, Outcome::Refused(_)))
        .unwrap();
    (sent.ask.ask_id, sent.ask.campaign) = (69902, "docs-site".into());
    let text = format!(
        "{}END-OF-DRAFT",
        "a long answer of the owner's own ".repeat(30)
    );
    sent.job = Job::Answer {
        ask_id: 69902,
        text,
        prompt: false,
        key: "tui-frame-kept".into(),
    };
}

/// The picker on the kept answer.
fn picker_kept(app: &mut App) {
    kept(app);
    app.picker();
    app.pick = Some(69902);
}

/// The kept answer, read in the review.
fn refused(app: &mut App) {
    kept(app);
    let sent = app.outcome(69902).cloned().unwrap();
    let crate::actions::Job::Answer { text, .. } = sent.job else {
        return;
    };
    app.answer(sent.ask);
    (app.text, app.editing, app.screen) = (text, false, Screen::Confirm);
}

/// The resend of an answer whose outcome was unknown, its ask still open.
fn resend(app: &mut App) {
    answered(app);
    let sent = app
        .outcomes
        .iter()
        .find(|s| s.state == crate::Outcome::Open)
        .cloned()
        .unwrap();
    app.answer(sent.ask);
    app.text = "A: yes".into();
    (app.editing, app.screen, app.resend) = (false, Screen::Confirm, true);
}

fn answer_gone(app: &mut App) {
    answer(app);
    let mut glance = app.data.glance.clone();
    glance.needs_you.remove(0);
    app.snapshot(glance);
}

fn dashboard(app: &mut App) {
    app.config.repos = vec!["example-org/app".into()];
    app.config.issues_repo = Some("example-org/app".into());
    app.config.health_repo = Some("example-org/app".into());
    app.config.panels[3..6].fill(true);
    app.observed_at = 1_792_000_000;
    app.mine = crate::prs::demo(app.observed_at);
    app.shipped = crate::github::demo_shipped(app.observed_at);
    app.builds = crate::github::demo_builds(app.observed_at);
    glance(app);
}

/// My PRs alone with the asks: the demo's invented login and rows through the real parsers,
/// the cursor on the first PR.
fn my_prs(app: &mut App) {
    dashboard(app);
    app.config.panels = [
        true, false, false, true, false, false, false, false, false, false, false,
    ];
    app.row = crate::glance::ids(&app.view())
        .iter()
        .position(|r| matches!(r, crate::glance::RowId::Pr(..)))
        .unwrap();
}

/// Panels 7-9 on, after `l` read the fixture's campaign three minutes ago.
fn ledger(app: &mut App) {
    dashboard(app);
    app.config.panels[6..9].fill(true);
    app.scope.read_at = Some(app.observed_at - 180);
}

/// Every card on with every source full: proposal §13 slice 2's all-eleven frames.
fn all_on(app: &mut App) {
    ledger(app);
    app.config.panels = [true; 11];
    app.issues = crate::github::demo_issues(app.observed_at);
    app.health = crate::health::sample(app.observed_at, crate::health::CLEAN);
}

/// All eleven on, the cursor on the first Build run: the viewport scrolled to it.
fn all_on_build(app: &mut App) {
    all_on(app);
    app.row = crate::glance::ids(&app.view())
        .iter()
        .position(|r| matches!(r, crate::glance::RowId::Run(..)))
        .unwrap();
}

/// All eleven on with only the first ask and Health holding rows: every card between them
/// empty or unavailable, each an inert stop. The cursor on the Shipped card's stop.
fn all_on_empty(app: &mut App) {
    all_on(app);
    let g = &mut app.data.glance;
    g.needs_you.truncate(1);
    g.attention.clear();
    g.campaigns.clear();
    g.quiet.count = 0;
    (app.mine.prs, app.mine.sample) = (vec![], crate::prs::Sample::default());
    app.shipped =
        crate::github::Dashboard::for_kind(&app.config.repos, crate::github::Kind::Shipped);
    app.shipped.sources[0].error = Some("GitHub unavailable");
    app.builds.sources.iter_mut().for_each(|s| s.runs.clear());
    app.issues = crate::github::Dashboard::for_kind(
        &["example-org/app".into()],
        crate::github::Kind::Issues,
    );
    app.issues.sources[0].error = Some("GitHub unavailable");
    app.scope.read_at = None;
    app.row = 4;
}

/// Issues (key 0) on with the demo pair, the cursor on its first group.
fn issues(app: &mut App) {
    dashboard(app);
    app.config.panels[9] = true;
    app.issues = crate::github::demo_issues(app.observed_at);
    first_group(app);
}

/// The cursor on the first Issues group, after any toggle that moves the rows.
fn first_group(app: &mut App) {
    app.row = crate::glance::ids(&app.view())
        .iter()
        .position(|r| matches!(r, crate::glance::RowId::Assignee(..)))
        .unwrap();
}

/// Health (key H) on with an invented hub note through the real parser, the cursor on its
/// row. Every other panel keeps its default.
fn health(app: &mut App) {
    health_note(app, crate::health::CLEAN);
}

fn health_note(app: &mut App, payload: &str) {
    dashboard(app);
    app.config.panels[10] = true;
    app.health = crate::health::sample(app.observed_at, payload);
    health_row(app);
}

/// The cursor on Health's row, after any toggle that moves the rows.
fn health_row(app: &mut App) {
    app.row = crate::glance::ids(&app.view())
        .iter()
        .position(|r| matches!(r, crate::glance::RowId::Health(..)))
        .unwrap();
}

/// The same with GitHub's panels off: what a narrow pane shows of the ledger's.
fn ledger_only(app: &mut App) {
    ledger(app);
    app.config.panels[3..6].fill(false);
}

fn glance(app: &mut App) {
    // docs-refresh changed since the owner last looked.
    app.changed = vec![4296];
}

fn stale(app: &mut App) {
    glance(app);
    app.fetch = Fetch {
        loaded: true,
        age_ms: 40_000,
        error: Some("hub unreachable".into()),
        in_flight: true,
        tick: 1,
        retry_in_s: 4,
        tries: 1,
        live: None,
        why: String::new(),
        began: None,
    };
}

fn campaign(app: &mut App) {
    app.screen = Screen::Campaign;
    app.lane = 6; // arch-plan-counter2, the ready lane
}

fn answer(app: &mut App) {
    app.answer(app.data.glance.needs_you[0].clone());
}

fn answer_long(app: &mut App) {
    // The 400-character ask from another machine: the hard case at 46 columns.
    app.answer(app.data.glance.needs_you[1].clone());
}

fn confirm(app: &mut App) {
    answer(app);
    app.screen = Screen::Confirm;
}

fn confirm_long(app: &mut App) {
    answer_long(app);
    app.screen = Screen::Confirm;
}

fn loading(app: &mut App) {
    app.data = Data::default();
    app.fetch = Fetch {
        in_flight: true,
        tick: 1,
        ..Fetch::default()
    };
}

fn no_data(app: &mut App) {
    app.data = Data::default();
    let error = Some("taskr: server unreachable (connection refused)".into());
    app.fetch = Fetch {
        error,
        tries: 3,
        retry_in_s: 4,
        ..Fetch::default()
    };
}

fn docs_pane(app: &mut App) {
    // The Docs pane focused and scrolled: 14 documents in a five-row box.
    campaign(app);
    app.pane = 2;
    app.scroll = 6;
}

fn slotr(app: &mut App) {
    app.screen = Screen::Slotr;
    app.slotr = Fetch {
        loaded: true,
        age_ms: 2000,
        ..Fetch::default()
    };
}

fn slotr_priority(app: &mut App) {
    // H1's fields: a priority waiter at the head of the runtime queue, and kinds.
    slotr(app);
    let runtime = app
        .data
        .slotr
        .pools
        .get_mut("runtime")
        .expect("a runtime pool");
    runtime.queue[0].priority = true.into();
    runtime.queue[0].kind = Some("service".into());
    let heavy = app.data.slotr.pools.get_mut("heavy").expect("a heavy pool");
    heavy.holders[0].kind = Some("test".into());
    heavy.queue[0].kind = Some("test".into());
    app.row = 3;
}

fn slotr_unavailable(app: &mut App) {
    // The last good pools, dimmed under the reason.
    slotr(app);
    app.slotr.error = Some(
        "slotr: no systemd user bus here; run it in a terminal with a systemd user session".into(),
    );
}

pub fn all() -> Vec<Spec> {
    let spec = |screen: &str, width, height, variant: &str, setup: fn(&mut App)| Spec {
        name: format!("{screen}-{width}x{height}{variant}"),
        width,
        height,
        light: variant.ends_with("-light"),
        setup,
    };
    vec![
        spec("dashboard", 120, 40, "", dashboard),
        spec("dashboard", 120, 40, "-light", dashboard),
        spec("dashboard", 46, 30, "", dashboard),
        spec("dashboard", 46, 30, "-light", dashboard),
        spec("dashboard", 46, 30, "-ascii", |app| {
            dashboard(app);
            app.ascii = true;
            app.theme = theme::PLAIN;
        }),
        spec("dashboard", 46, 30, "-hidden", |app| {
            dashboard(app);
            app.config.panels[0] = false;
        }),
        spec("dashboard", 46, 30, "-off", |app| {
            app.config.panels = [false; 11];
            app.config_warning = Some("TUI config not saved; choices kept for this session".into());
        }),
        spec("dashboard", 46, 30, "-empty-ledger", |app| {
            dashboard(app);
            app.data = Data::default();
        }),
        spec("dashboard", 46, 30, "-failed-ledger", |app| {
            dashboard(app);
            no_data(app);
        }),
        spec("dashboard", 120, 40, "-stale-ledger", |app| {
            dashboard(app);
            stale(app);
        }),
        spec("dashboard", 46, 30, "-stale-prs", |app| {
            dashboard(app);
            app.mine.error = Some("GitHub unavailable");
        }),
        spec("dashboard", 46, 30, "-stale-shipped", |app| {
            dashboard(app);
            app.shipped.sources[0].error = Some("GitHub unavailable");
        }),
        spec("dashboard", 120, 40, "-stale-builds", |app| {
            dashboard(app);
            app.builds.sources[0].error = Some("GitHub unavailable");
        }),
        spec("dashboard", 46, 30, "-failed-sources", |app| {
            dashboard(app);
            app.shipped =
                crate::github::Dashboard::for_kind(&app.config.repos, crate::github::Kind::Shipped);
            app.builds =
                crate::github::Dashboard::for_kind(&app.config.repos, crate::github::Kind::Build);
            app.shipped.sources[0].error = Some("GitHub unavailable");
            app.builds.sources[0].error = Some("GitHub authentication required");
        }),
        spec("dashboard", 80, 30, "", dashboard),
        spec("dashboard", 46, 20, "", dashboard),
        spec("dashboard", 30, 10, "", dashboard),
        spec("board", 120, 40, "-on", all_on),
        spec("board", 120, 40, "-on-light", all_on),
        spec("board", 80, 30, "-on", all_on),
        spec("board", 46, 30, "-on", all_on),
        spec("board", 46, 20, "-on", all_on),
        spec("board", 46, 20, "-on-build", all_on_build),
        spec("board", 46, 20, "-on-ascii", |app| {
            all_on_build(app);
            app.ascii = true;
            app.theme = theme::PLAIN;
        }),
        spec("board", 46, 20, "-on-empty", all_on_empty),
        spec("board", 40, 12, "-on", all_on_build),
        spec("board", 40, 12, "-on-light", all_on_build),
        spec("detail", 120, 40, "-shipped", |app| {
            dashboard(app);
            inspect(app, |r| matches!(r, crate::glance::RowId::Author(_)));
        }),
        spec("detail", 120, 40, "-build", |app| {
            dashboard(app);
            inspect(app, |r| matches!(r, crate::glance::RowId::Run(..)));
        }),
        spec("detail", 46, 30, "-pr", |app| {
            dashboard(app);
            inspect(app, |r| matches!(r, crate::glance::RowId::Pr(..)));
        }),
        spec("detail", 120, 40, "-pr", |app| {
            dashboard(app);
            inspect(app, |r| matches!(r, crate::glance::RowId::Pr(..)));
        }),
        spec("detail", 46, 30, "-pr-capped", |app| {
            dashboard(app);
            inspect(app, |r| matches!(r, crate::glance::RowId::Pr(_, 39)));
        }),
        spec("detail", 46, 30, "-pr-head-moved", |app| {
            dashboard(app);
            inspect(app, |r| matches!(r, crate::glance::RowId::Pr(_, 38)));
        }),
        spec("detail", 46, 30, "-pr-unreadable", |app| {
            dashboard(app);
            inspect(app, |r| matches!(r, crate::glance::RowId::Pr(_, 37)));
        }),
        spec("prs", 120, 40, "", my_prs),
        spec("prs", 120, 40, "-light", my_prs),
        spec("prs", 100, 40, "", my_prs),
        spec("prs", 80, 30, "", my_prs),
        spec("prs", 46, 30, "", my_prs),
        spec("prs", 46, 30, "-light", my_prs),
        spec("prs", 46, 30, "-ascii", |app| {
            my_prs(app);
            app.ascii = true;
            app.theme = theme::PLAIN;
        }),
        spec("prs", 46, 20, "", my_prs),
        spec("prs", 120, 40, "-notes", |app| {
            my_prs(app);
            let s = &mut app.mine.sample;
            (s.found, s.closed, s.outside, s.partial) = (45, 1, 1, true);
        }),
        spec("prs", 120, 40, "-stale", |app| {
            my_prs(app);
            app.mine.error = Some("GitHub rate limited");
        }),
        spec("prs", 80, 30, "-zero", |app| {
            my_prs(app);
            (app.mine.prs, app.mine.sample) = (vec![], crate::prs::Sample::default());
        }),
        spec("prs", 80, 30, "-login-failed", |app| {
            my_prs(app);
            app.mine = crate::prs::Mine::new(&app.config.repos);
            app.mine.error = Some("GitHub authentication required");
        }),
        spec("prs", 80, 30, "-login-reading", |app| {
            my_prs(app);
            app.mine = crate::prs::Mine::new(&app.config.repos);
        }),
        spec("prs", 120, 40, "-repos", |app| {
            my_prs(app);
            app.mine.repos.push("demo-org/tools".into());
            app.mine.prs[1].repo = "demo-org/tools".into();
        }),
        spec("detail", 46, 30, "-campaign", |app| {
            glance(app);
            inspect(app, |r| matches!(r, crate::glance::RowId::Campaign(_)));
        }),
        spec("detail", 80, 30, "-check", |app| {
            glance(app);
            inspect(app, |r| matches!(r, crate::glance::RowId::Check(..)));
        }),
        spec("detail", 46, 30, "-empty", |app| {
            app.screen = Screen::Detail
        }),
        spec("detail", 46, 30, "-gone", |app| {
            // The ask was answered elsewhere after I opened it.
            detail(app);
            app.data.glance.needs_you.pop();
        }),
        spec("detail", 120, 40, "", detail),
        spec("detail", 120, 40, "-light", detail),
        spec("detail", 80, 30, "", detail),
        spec("detail", 46, 30, "", detail),
        spec("detail", 46, 30, "-light", detail),
        spec("detail", 46, 30, "-ascii", |app| {
            detail(app);
            app.ascii = true;
            app.theme = theme::PLAIN;
        }),
        spec("detail", 46, 20, "", detail),
        spec("detail", 30, 10, "", detail),
        spec("asks", 120, 40, "", asks),
        spec("asks", 120, 40, "-light", asks),
        spec("asks", 80, 30, "", asks),
        spec("asks", 46, 30, "", asks),
        spec("asks", 46, 30, "-light", asks),
        spec("asks", 46, 30, "-ascii", |app| {
            asks(app);
            app.ascii = true;
            app.theme = theme::PLAIN;
        }),
        spec("asks", 46, 20, "", asks),
        spec("asks", 30, 10, "", asks),
        spec("asks", 46, 30, "-hidden", |app| {
            // Panel 1 hidden on the Dashboard: Asks still lists every open ask.
            asks(app);
            app.config.panels[0] = false;
        }),
        spec("asks", 46, 30, "-empty", |app| {
            app.screen = Screen::Asks;
            app.data.glance.needs_you.clear();
        }),
        spec("fleet", 120, 40, "", fleet),
        spec("fleet", 120, 40, "-light", fleet),
        spec("fleet", 80, 30, "", fleet),
        spec("fleet", 46, 30, "", fleet),
        spec("fleet", 46, 30, "-light", fleet),
        spec("fleet", 46, 30, "-ascii", |app| {
            fleet(app);
            app.ascii = true;
            app.theme = theme::PLAIN;
        }),
        spec("fleet", 46, 20, "", fleet),
        spec("fleet", 30, 10, "", fleet),
        spec("fleet", 46, 30, "-hidden", |app| {
            // Panel 3 hidden on the Dashboard: Fleet still lists every listed campaign.
            fleet(app);
            app.config.panels[2] = false;
        }),
        spec("fleet", 40, 12, "", fleet),
        spec("fleet", 120, 40, "-read", fleet_read),
        spec("fleet", 120, 40, "-read-light", fleet_read),
        spec("fleet", 80, 30, "-read", fleet_read),
        spec("fleet", 46, 30, "-read", fleet_read),
        spec("fleet", 46, 30, "-read-light", fleet_read),
        spec("fleet", 46, 30, "-read-ascii", |app| {
            fleet_read(app);
            app.ascii = true;
            app.theme = theme::PLAIN;
        }),
        spec("fleet", 120, 40, "-read-plain", |app| {
            // NO_COLOR: the terminal's own colours.
            fleet_read(app);
            app.theme = theme::PLAIN;
        }),
        spec("fleet", 46, 20, "-read", fleet_read),
        spec("fleet", 40, 12, "-read", fleet_read),
        spec("fleet", 120, 40, "-reading", |app| {
            // `l` on search-index, still on its way: the lanes on hand stay, named.
            fleet_read(app);
            app.row = 0;
            app.scope.want = Some((4310, Screen::Fleet, 0));
        }),
        spec("fleet", 120, 40, "-failed", |app| {
            // The re-read failed: the last good read stays, with its age and the error.
            fleet_read(app);
            app.scope.error = Some("reading tui-frames failed: hub unreachable".into());
        }),
        spec("fleet", 46, 30, "-failed-first", |app| {
            // The first read failed: nothing cached, the error said.
            fleet(app);
            app.scope.error = Some("reading search-index failed: hub unreachable".into());
        }),
        spec("fleet", 120, 40, "-missing", |app| {
            // Counts the snapshot did not carry: `?`, and totals that say so.
            fleet(app);
            app.data.glance.campaigns[0].lanes.known = [true, false, true];
            app.data.glance.campaigns[6].lanes.known = [false; 3];
        }),
        spec("fleet", 46, 30, "-missing", |app| {
            fleet(app);
            app.data.glance.campaigns[0].lanes.known = [true, false, true];
            app.data.glance.campaigns[6].lanes.known = [false; 3];
        }),
        spec("fleet", 120, 40, "-filter", |app| {
            fleet_read(app);
            app.filter = "doc".into();
            app.row = 0;
        }),
        spec("fleet", 80, 30, "-quiet-only", |app| {
            // Nothing listed, two quiet: never a fleet-wide idle claim.
            fleet(app);
            app.data.glance.campaigns.clear();
            app.row = 0;
        }),
        spec("fleet", 120, 40, "-stale", |app| {
            stale(app);
            app.screen = Screen::Fleet;
            app.row = 2;
            app.scope.read_at = Some(app.observed_at - 40);
        }),
        spec("fleet", 46, 30, "-read-long", fleet_read_long),
        spec("fleet", 40, 12, "-read-long", fleet_read_long),
        spec("fleet", 120, 40, "-read-root", |app| {
            fleet_read(app);
            app.row = 6;
        }),
        spec("fleet", 120, 40, "-read-orphan", |app| {
            fleet_read(app);
            if let Some(l) = app.data.campaign.lanes.iter_mut().find(|l| l.id == 4190) {
                l.status = "closed".into();
            }
        }),
        spec("detail", 46, 30, "-fleet-remote", |app| {
            fleet(app);
            if let Some(c) = app.data.glance.campaigns.iter_mut().find(|c| c.id == 4087) {
                c.lanes.known = [true, false, true];
            }
            app.row = 5;
            app.inspect();
        }),
        spec("issues", 120, 40, "", issues),
        spec("issues", 120, 40, "-light", issues),
        spec("issues", 46, 30, "", issues),
        spec("issues", 46, 30, "-light", issues),
        spec("issues", 46, 20, "", issues),
        spec("issues", 46, 30, "-empty", |app| {
            issues(app);
            let s = &mut app.issues.sources[0];
            (s.issues, s.total, s.closed_total) = (vec![], 0, 0);
            app.row = 0;
        }),
        spec("issues", 46, 30, "-unavailable", |app| {
            issues(app);
            app.issues = crate::github::Dashboard::for_kind(
                &["example-org/app".into()],
                crate::github::Kind::Issues,
            );
            app.issues.sources[0].error = Some("GitHub unavailable");
            app.row = 0;
        }),
        spec("issues", 46, 30, "-stale", |app| {
            issues(app);
            app.issues.sources[0].error = Some("GitHub rate limited");
        }),
        spec("issues", 120, 40, "-overflow", |app| {
            // Every ledger panel on: Issues keeps its minimum below them, then scrolls.
            issues(app);
            app.config.panels[6..10].fill(true);
            app.scope.read_at = Some(app.observed_at - 180);
            app.issues.sources[0].total = 100;
            app.issues.sources[0].closed_total = 100;
            first_group(app);
        }),
        spec("issues", 120, 40, "-overflow-detail", |app| {
            // The same group in Detail: its sample says what it may have left out.
            issues(app);
            app.config.panels[6..10].fill(true);
            app.scope.read_at = Some(app.observed_at - 180);
            app.issues.sources[0].total = 100;
            app.issues.sources[0].closed_total = 100;
            first_group(app);
            app.inspect();
        }),
        spec("issues", 46, 30, "-cap", |app| {
            // Issues alone, both reads at their 100-row cap: the list says so for each.
            issues(app);
            app.config.panels[..9].fill(false);
            app.issues.sources[0].total = 100;
            app.issues.sources[0].closed_total = 100;
            first_group(app);
        }),
        spec("health", 120, 40, "", health),
        spec("health", 120, 40, "-light", health),
        spec("health", 46, 30, "", health),
        spec("health", 46, 30, "-light", health),
        spec("health", 46, 20, "", health),
        spec("health", 46, 30, "-pending", |app| {
            health_note(
                app,
                &crate::health::CLEAN
                    .replace("deploy=live", "deploy=pending")
                    .replace("sentry=clean", "sentry=pending"),
            );
        }),
        spec("health", 46, 30, "-failed", |app| {
            health_note(
                app,
                &crate::health::CLEAN.replace("deploy=live", "deploy=failed"),
            );
        }),
        spec("health", 120, 40, "-new", |app| {
            health_note(
                app,
                &crate::health::CLEAN.replace("sentry=clean", "sentry=new:TRIP-12,TRIP-14"),
            );
        }),
        spec("health", 46, 30, "-no-note", |app| {
            health_note(app, "deploy check started");
        }),
        spec("health", 46, 30, "-malformed", |app| {
            health_note(app, "HEALTH repo=example-org/app env=production");
        }),
        spec("health", 46, 30, "-stale", |app| {
            // Controlled export: the last report kept after a failed read.
            health(app);
            app.health.error = Some(crate::health::UNAVAILABLE);
        }),
        spec("health", 46, 20, "-stale", |app| {
            // Narrow and short: the compact row says stale before anything it clips.
            health(app);
            app.health.error = Some(crate::health::UNAVAILABLE);
        }),
        spec("health", 46, 30, "-unavailable", |app| {
            // The first read failed: unknown, never a report.
            health(app);
            let (repo, root) = (app.health.repo.take(), app.health.root);
            app.health = crate::health::Health::default();
            (app.health.repo, app.health.root) = (repo, root);
            app.health.error = Some(crate::health::UNAVAILABLE);
        }),
        spec("health", 120, 40, "-overflow", |app| {
            // Every panel on: Health keeps its place last, after Issues.
            health(app);
            app.config.panels[6..].fill(true);
            app.scope.read_at = Some(app.observed_at - 180);
            app.issues = crate::github::demo_issues(app.observed_at);
            health_row(app);
        }),
        spec("ledger", 120, 40, "", |app| {
            ledger(app);
            app.row = crate::glance::ids(&app.view())
                .iter()
                .position(|r| matches!(r, crate::glance::RowId::Decision(..)))
                .unwrap();
        }),
        spec("ledger", 120, 40, "-light", ledger),
        spec("ledger", 46, 30, "", ledger_only),
        spec("ledger", 46, 30, "-light", ledger_only),
        spec("ledger", 46, 20, "", ledger_only),
        spec("ledger", 46, 30, "-unscoped", |app| {
            ledger_only(app);
            app.scope.read_at = None;
        }),
        spec("ledger", 46, 30, "-stale", |app| {
            // The glance is stale and the last re-read failed: both say so, nothing is zero.
            ledger_only(app);
            stale(app);
            app.scope.error = Some("reading tui-frames failed: hub unreachable".into());
            app.data.glance.campaigns[0].lanes.known = [true, false, false];
        }),
        spec("ledger", 46, 30, "-fleet", |app| {
            // Campaigns hidden, Fleet shown from the same snapshot.
            ledger_only(app);
            app.config.panels[2] = false;
            app.config.panels[6] = false;
            app.config.panels[8] = false;
        }),
        spec("glance", 46, 30, "", glance),
        spec("glance", 70, 30, "", glance),
        spec("glance", 120, 40, "", glance),
        spec("detail", 120, 40, "-end", detail_end),
        spec("glance", 46, 20, "", glance),
        spec("glance", 46, 30, "-stale", stale),
        spec("glance", 24, 1, "", glance),
        spec("glance", 46, 30, "-ascii", |app| {
            glance(app);
            app.ascii = true;
        }),
        spec("glance", 46, 30, "-loading", loading),
        spec("glance", 46, 30, "-nodata", no_data),
        spec("glance", 46, 30, "-empty", |app| app.data = Data::default()),
        spec("campaign", 120, 40, "-docs", docs_pane),
        spec("all", 46, 30, "-scrolled", |app| {
            app.screen = Screen::AllCampaigns;
            app.row = 27;
        }),
        spec("glance", 46, 30, "-light", glance),
        spec("glance", 120, 40, "-light", glance),
        spec("campaign", 46, 30, "", campaign),
        spec("campaign", 120, 40, "", campaign),
        spec("campaign", 46, 30, "-light", campaign),
        spec("campaign", 120, 40, "-light", campaign),
        spec("glance", 46, 30, "-structured", structured),
        spec("glance", 70, 30, "-structured", structured),
        spec("glance", 120, 40, "-structured", structured),
        spec("glance", 46, 20, "-structured", structured),
        spec("answer", 120, 40, "-gone", answer_gone),
        spec("answer", 46, 30, "-gone", answer_gone),
        spec("picker", 120, 40, "", picker),
        spec("picker", 120, 40, "-light", picker),
        spec("picker", 80, 30, "", picker),
        spec("picker", 46, 30, "", picker),
        spec("picker", 46, 30, "-ascii", |app| {
            picker(app);
            app.ascii = true;
            app.theme = theme::PLAIN;
        }),
        spec("picker", 46, 20, "", picker),
        spec("picker", 80, 30, "-empty", |app| {
            fleet(app);
            app.data.glance.needs_you.clear();
            app.picker();
        }),
        spec("asks", 46, 30, "-one", |app| {
            asks(app);
            app.data.glance.needs_you.truncate(1);
            app.row = 0;
        }),
        spec("asks", 120, 40, "-scrolled", |app| {
            asks(app);
            app.data.glance.needs_you.insert(0, long_ask());
            app.row = 0;
            app.scroll = 6;
        }),
        spec("asks", 120, 40, "-answered", answered),
        spec("asks", 46, 30, "-answered", answered),
        spec("confirm", 120, 40, "-resend", resend),
        spec("picker", 120, 40, "-kept", picker_kept),
        spec("picker", 46, 30, "-kept", picker_kept),
        spec("confirm", 120, 40, "-refused", refused),
        spec("confirm", 46, 30, "-refused", refused),
        spec("confirm", 46, 30, "-resend", resend),
        spec("answer", 46, 30, "-single", answer_single),
        spec("answer", 70, 30, "-single", answer_single),
        spec("answer", 120, 40, "-single", answer_single),
        spec("answer", 46, 20, "-single", answer_single),
        spec("answer", 46, 30, "-multi", answer_multi),
        spec("answer", 70, 30, "-multi", answer_multi),
        spec("answer", 120, 40, "-multi", answer_multi),
        spec("answer", 46, 20, "-multi", answer_multi),
        spec("answer", 46, 30, "", answer_long),
        spec("answer", 120, 40, "", answer),
        spec("confirm", 46, 30, "", confirm_long),
        spec("confirm", 120, 40, "", confirm),
        spec("help", 46, 30, "", |app| app.screen = Screen::Help),
        spec("help", 120, 40, "", |app| app.screen = Screen::Help),
        spec("help", 100, 30, "", |app| app.screen = Screen::Help),
        spec("all", 46, 30, "", |app| app.screen = Screen::AllCampaigns),
        spec("all", 120, 40, "", |app| app.screen = Screen::AllCampaigns),
        spec("pager", 46, 30, "", |app| app.screen = Screen::Pager),
        spec("pager", 120, 40, "", |app| app.screen = Screen::Pager),
        spec("slotr", 80, 24, "", slotr),
        spec("slotr", 120, 40, "", slotr),
        spec("slotr", 120, 40, "-priority", slotr_priority),
        spec("slotr", 46, 30, "", slotr),
        spec("slotr", 80, 24, "-unavailable", slotr_unavailable),
        spec("slotr", 80, 24, "-unavailable-first", |app| {
            // The first read failed: no pools, no spinner, the reason.
            app.screen = Screen::Slotr;
            app.slotr.error = Some(
                "slotr: no systemd user bus here; run it in a terminal with a user session".into(),
            );
        }),
        spec("slotr", 80, 24, "-loading", |app| {
            app.screen = Screen::Slotr;
        }),
    ]
}

pub fn render(spec: &Spec) -> Buffer {
    let mut app = App::new(fixture());
    app.config.repos = vec!["example-org/app".into()];
    app.config.issues_repo = Some("example-org/app".into());
    app.config.health_repo = Some("example-org/app".into());
    app.config.panels[3..6].fill(true);
    app.fetch.age_ms = 2000;
    app.fetch.loaded = true;
    if spec.light {
        app.theme = theme::LIGHT;
    }
    (spec.setup)(&mut app);
    let mut terminal =
        Terminal::new(TestBackend::new(spec.width, spec.height)).expect("a test backend");
    terminal.draw(|f| draw(f, &app)).expect("a frame");
    terminal.backend().buffer().clone()
}

/// The frame as plain text, trailing spaces trimmed: what the golden files hold.
pub fn text(buf: &Buffer) -> String {
    let mut out = String::new();
    for y in 0..buf.area.height {
        let line: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
        out += line.trim_end();
        out.push('\n');
    }
    out
}

/// The frame as truecolor ANSI, one SGR per cell: `cat` shows it in a real terminal, and
/// `tools/png.py` paints it.
pub fn ansi(buf: &Buffer) -> String {
    let color = |c: Color, base: u8| match c {
        Color::Rgb(r, g, b) => format!("{};2;{r};{g};{b}", base + 8),
        _ => format!("{}", base + 9),
    };
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            let cell = &buf[(x, y)];
            let bold = if cell.modifier.contains(Modifier::BOLD) {
                ";1"
            } else {
                ""
            };
            out += &format!(
                "\x1b[0;{};{}{bold}m{}",
                color(cell.fg, 30),
                color(cell.bg, 40),
                cell.symbol()
            );
        }
        out += "\x1b[0m\n";
    }
    out
}
