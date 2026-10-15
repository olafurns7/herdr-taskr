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

/// The fixture's 12-step ask, longer than the glance's detail pane. It is kept out of the
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
    // The single-select selected, so the wide frames show its options in the detail pane.
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

fn detail(app: &mut App) {
    // The long ask selected, the detail focused and scrolled to its end (`tab` past the
    // last section, then `G`; the draw stops at the last line).
    app.data.glance.needs_you.push(long_ask());
    (app.row, app.detail, app.scroll) = (2, true, usize::MAX);
}

fn dashboard(app: &mut App) {
    app.observed_at = 1_792_000_000;
    app.github = crate::github::demo();
    for (i, pr) in app.github.sources[0].prs.iter_mut().enumerate() {
        pr.created = app.observed_at - (i as i64 + 1) * 3600;
    }
    app.shipped = crate::github::demo_shipped(app.observed_at);
    app.builds = crate::github::demo_builds(app.observed_at);
    glance(app);
}

/// Panels 7-9 on, after `l` read the fixture's campaign three minutes ago.
fn ledger(app: &mut App) {
    dashboard(app);
    app.config.panels[6..9].fill(true);
    app.scope.read_at = Some(app.observed_at - 180);
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
        light: variant == "-light",
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
            app.github.sources[0].error = Some("GitHub unavailable");
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
        spec("dashboard", 120, 40, "-shipped-detail", |app| {
            dashboard(app);
            app.row = crate::glance::ids(&app.view())
                .iter()
                .position(|r| matches!(r, crate::glance::RowId::Author(_)))
                .unwrap();
        }),
        spec("dashboard", 120, 40, "-build-detail", |app| {
            dashboard(app);
            app.row = crate::glance::ids(&app.view())
                .iter()
                .position(|r| matches!(r, crate::glance::RowId::Run(..)))
                .unwrap();
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
                &[crate::github::ISSUES_REPO.into()],
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
            health_note(app, "HEALTH repo=trip-to-japan/trip env=production");
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
            app.health = crate::health::Health::default();
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
        spec("glance", 120, 40, "-detail", detail),
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
