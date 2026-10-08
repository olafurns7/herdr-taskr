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

pub fn all() -> Vec<Spec> {
    let spec = |screen: &str, width, height, variant: &str, setup: fn(&mut App)| Spec {
        name: format!("{screen}-{width}x{height}{variant}"),
        width,
        height,
        light: variant == "-light",
        setup,
    };
    vec![
        spec("glance", 46, 30, "", glance),
        spec("glance", 70, 30, "", glance),
        spec("glance", 120, 40, "", glance),
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
