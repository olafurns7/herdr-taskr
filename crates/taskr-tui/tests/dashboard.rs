//! Source freshness is independent even when the ledger has no usable snapshot.
use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};
use taskr_tui::{App, Fetch, draw, frames, github, model::Data};

fn render(app: &App, width: u16, height: u16) -> (String, Buffer) {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|f| draw(f, app)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    (frames::text(&buffer), buffer)
}

#[test]
fn fresh_prs_render_with_empty_failed_or_stale_ledger() {
    for width in [46, 120] {
        for state in 0..3 {
            let mut app = App::new(if state == 2 {
                frames::fixture()
            } else {
                Data::default()
            });
            app.github = github::demo();
            app.fetch = Fetch {
                loaded: state != 1,
                error: (state > 0).then(|| "hub unreachable".into()),
                ..Fetch::default()
            };
            let (text, buffer) = render(&app, width, 30);
            assert!(
                text.contains("failing #42") && text.contains("@alex"),
                "{state}/{width}:\n{text}"
            );
            assert!(text.contains("3 open"), "{text}");
            assert_eq!(
                buffer
                    .content
                    .iter()
                    .find(|c| c.symbol() == "■")
                    .unwrap()
                    .fg,
                app.theme.need
            );
            if state == 1 {
                assert!(text.contains("no data"));
            }
            if state == 2 {
                assert!(text.contains("stale") && text.contains("2 need you"));
            }
        }
    }
}

#[test]
fn failed_pr_read_does_not_stale_ledger_or_invent_zero_totals() {
    let mut app = App::new(frames::fixture());
    app.fetch.loaded = true;
    app.github = github::demo();
    app.github.sources[0].error = Some("GitHub unavailable");
    let (text, _) = render(&app, 46, 30);
    let header = text.lines().next().unwrap();
    assert!(
        header.contains("2 need you") && !header.contains("stale"),
        "{text}"
    );
    assert!(
        text.contains("stale") && text.contains("failing #42"),
        "{text}"
    );
    app.github = github::Dashboard::new(&["trip-to-japan/trip".into()]);
    app.github.sources[0].error = Some("GitHub authentication required");
    let (text, _) = render(&app, 46, 30);
    assert!(
        text.contains("CI counts unavailable") && text.contains("unavailable"),
        "{text}"
    );
    assert!(
        !text.contains("0 open") && !text.contains("0 fail"),
        "{text}"
    );
}

#[test]
fn filter_does_not_change_source_totals_and_plain_frames_keep_status_words() {
    let mut app = App::new(frames::fixture());
    app.fetch.loaded = true;
    app.github = github::demo();
    app.filter = "no matches".into();
    let (text, _) = render(&app, 46, 30);
    assert!(
        text.contains("3 open") && text.contains("2 need you"),
        "{text}"
    );
    app.filter.clear();
    app.ascii = true;
    app.theme = taskr_tui::theme::PLAIN;
    let (text, buffer) = render(&app, 46, 30);
    assert!(text.is_ascii(), "{text}");
    assert!(
        text.contains("# failing #42") && text.contains("running #41"),
        "{text}"
    );
    assert!(
        buffer
            .content
            .iter()
            .all(|c| c.fg == ratatui::style::Color::Reset && c.bg == ratatui::style::Color::Reset)
    );
}

#[test]
fn sample_limits_and_check_truncation_are_visible_at_narrow_width() {
    let mut app = App::new(frames::fixture());
    app.fetch.loaded = true;
    app.github = github::demo();
    app.github.sources[0].total = 100;
    app.github.sources[0].prs[0].truncated = true;
    let (text, _) = render(&app, 46, 30);
    assert!(text.contains("first 100; more may exist"), "{text}");
    assert!(text.contains("failing (truncated) #42"), "{text}");
}

#[test]
fn prs_use_empty_ledger_space_without_hiding_truth_placeholder() {
    for state in 0..3 {
        let mut app = App::new(if state == 2 {
            frames::fixture()
        } else {
            Data::default()
        });
        app.fetch.loaded = state != 1;
        app.fetch.error = (state == 1).then(|| "hub unreachable".into());
        if state == 2 {
            app.config.panels = [false, false, false, true];
        }
        app.github = github::demo();
        let pr = app.github.sources[0].prs[0].clone();
        app.github.sources[0].prs = (1..=12)
            .map(|number| {
                let mut p = pr.clone();
                p.number = number;
                p
            })
            .collect();
        app.github.sources[0].total = 12;
        for (width, height) in [(46, 30), (120, 40)] {
            let (text, _) = render(&app, width, height);
            assert!(text.matches("■ ").count() > 3, "{state}: {text}");
            let placeholder = [
                "No open campaigns",
                "Cannot reach the hub",
                "No visible ledger rows",
            ][state];
            assert!(text.contains(placeholder), "{state}: {text}");
        }
    }
}

#[test]
fn review_metadata_retry_and_freshness_remain_readable() {
    let mut app = App::new(Data::default());
    app.fetch.loaded = true;
    app.github = github::demo();
    app.theme = taskr_tui::theme::LIGHT;
    let (text, buffer) = render(&app, 120, 40);
    assert!(text.contains("approved"), "{text}");
    for line in text
        .lines()
        .filter(|l| l.contains("@alex") || l.contains("@sam"))
    {
        assert!(!line.trim_end_matches([' ', '│']).ends_with('·'), "{line}");
    }
    let (y, line) = text
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains("3 open;"))
        .unwrap();
    let x = line.find("3 open;").unwrap();
    assert_eq!(buffer[(x as u16, y as u16)].fg, app.theme.text);
    app.fetch.loaded = false;
    app.fetch.error = Some("hub unreachable".into());
    let (text, _) = render(&app, 46, 30);
    let footer = text.lines().last().unwrap();
    assert_eq!(footer.matches("r retry").count(), 1, "{footer}");
    assert!(!footer.contains("r refresh"), "{footer}");
}
