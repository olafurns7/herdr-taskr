//! Source freshness is independent even when the ledger has no usable snapshot.
use ratatui::{
    Terminal,
    backend::TestBackend,
    buffer::Buffer,
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
};
use taskr_tui::{App, Fetch, draw, frames, github, input, model::Data};

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
            app.config.panels = [
                false, false, false, true, false, false, false, false, false, false, false,
            ];
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

#[test]
fn new_sources_render_independently_and_errors_never_imply_zero() {
    for width in [46, 120] {
        for state in 0..3 {
            let mut app = App::new(if state == 2 {
                frames::fixture()
            } else {
                Data::default()
            });
            app.fetch.loaded = state != 1;
            app.fetch.error = (state > 0).then(|| "hub unreachable".into());
            // Give each panel enough space to exercise provenance, rows and the ledger placeholder.
            app.shipped = github::demo_shipped(app.observed_at);
            let (text, _) = render(&app, width, 40);
            assert!(
                text.contains("SHIPPED")
                    && text.contains("4 observed merged")
                    && text.contains("@alex"),
                "{text}"
            );
            app.shipped = github::Dashboard::default();
            app.builds = github::demo_builds(app.observed_at);
            let (text, _) = render(&app, width, 40);
            assert!(
                text.contains("BUILD QUEUE")
                    && text.contains("running (in_progress)")
                    && text.contains("latest100-run sample"),
                "{text}"
            );
        }
    }
    for kind in [github::Kind::Shipped, github::Kind::Build] {
        let mut app = App::new(Data::default());
        app.fetch.loaded = true;
        let mut d = github::Dashboard::for_kind(&["a/b".into()], kind);
        d.sources[0].error = Some("GitHub authentication required");
        if kind == github::Kind::Shipped {
            app.shipped = d;
        } else {
            app.builds = d;
        }
        let (text, _) = render(&app, 46, 30);
        assert!(
            text.contains("Counts unavailable") && text.contains("unavailable"),
            "{text}"
        );
        assert!(
            !text.contains("0 active") && !text.contains("0 observed"),
            "{text}"
        );
    }
}

#[test]
fn retained_samples_keep_independent_stale_and_coverage_copy() {
    let mut app = App::new(Data::default());
    app.fetch.loaded = true;
    app.shipped = github::demo_shipped(app.observed_at);
    app.shipped.sources[0].total = 100;
    app.shipped.sources[0].error = Some("GitHub unavailable");
    let (text, _) = render(&app, 46, 30);
    assert!(
        text.contains("first 100; more may exist") && text.contains("stale"),
        "{text}"
    );
    for s in &mut app.shipped.sources {
        for p in &mut s.merged {
            p.merged = app.observed_at - 86401;
        }
    }
    let (text, _) = render(&app, 46, 30);
    assert!(
        text.contains("first 100; more may exist") && text.contains("None observed"),
        "{text}"
    );
    app.shipped = github::Dashboard::default();
    app.builds = github::demo_builds(app.observed_at);
    app.builds.sources[0].runs.clear();
    let (text, _) = render(&app, 46, 30);
    assert!(
        text.contains("latest100-run sample") && text.contains("None active observed"),
        "{text}"
    );
    assert!(!text.contains("idle"), "{text}");
}

#[test]
fn build_height_includes_the_sample_line_and_both_observed_rows() {
    let mut app = App::new(Data::default());
    app.fetch.loaded = true;
    app.builds = github::demo_builds(app.observed_at);
    let (text, _) = render(&app, 46, 30);
    assert!(
        text.contains("latest100-run sample")
            && text.contains("running (in_progress)")
            && text.contains("queued (queued)"),
        "{text}"
    );
    // Nothing cut: no overflow line (slice 2's "+N more …", before it "outside view").
    assert!(!text.contains(" more "), "{text}");
}

#[test]
fn spare_panel_space_shows_priority_prs_and_keeps_later_source_status() {
    for (width, height) in [(46, 20), (46, 30), (120, 40)] {
        for failed in [false, true] {
            let mut app = App::new(frames::fixture());
            app.fetch.loaded = true;
            app.github = github::demo();
            app.shipped = github::demo_shipped(app.observed_at);
            app.builds = github::demo_builds(app.observed_at);
            if failed {
                app.shipped = github::Dashboard::for_kind(&app.config.repos, github::Kind::Shipped);
                app.builds = github::Dashboard::for_kind(&app.config.repos, github::Kind::Build);
                app.shipped.sources[0].error = Some("GitHub unavailable");
                app.builds.sources[0].error = Some("GitHub authentication required");
            }
            let (text, _) = render(&app, width, height);
            // Priority first: asks, checks and the first PR are on the first screen.
            assert!(
                text.contains("OPEN PRS")
                    && text.contains("3 open")
                    && text.contains("failing #42"),
                "{width}x{height}/{failed}: {text}"
            );
            if height == 20 {
                assert!(
                    !text.contains("BUILD QUEUE"),
                    "later kind displaced priority panel: {text}"
                );
            }
            // No PR silently dropped: each is drawn or counted on the card's more line.
            let missing = ["failing #42", "running #41", "checks passing #40"]
                .iter()
                .filter(|pr| !text.contains(**pr))
                .count();
            let more = format!("+{missing} more PR");
            assert!(
                (missing == 0 && !text.contains("more PRs")) || text.contains(&more),
                "{width}x{height}/{failed}: {text}"
            );
            if width == 120 {
                assert_eq!(missing, 0, "{text}");
            }
            // The PR card is not padded with blank lines.
            let lines: Vec<&str> = text.lines().collect();
            let start = lines.iter().position(|l| l.contains("OPEN PRS")).unwrap();
            let blank = |l: &str| l.trim_matches([' ', '│']).is_empty();
            let mut end = lines[start + 1..]
                .iter()
                .position(|l| l.contains(" ──") || l.starts_with(" ↓"))
                .map_or(lines.len() - 1, |i| start + 1 + i);
            // The viewport may end early above the ↓ line; that row is not the card's.
            while blank(lines[end - 1]) {
                end -= 1;
            }
            assert!(
                lines[start..end].iter().all(|l| !blank(l)),
                "{width}x{height}/{failed}: {text}"
            );
            // Later cards: in view, or disclosed below and reached by Tab, with the same
            // source truth either way.
            let (shipped, build) = if text.lines().any(|l| l.starts_with(" ↓")) {
                // "↓ 1 card below: BUILD QUEUE 2 · tab" or "↓ 8 cards below: … · …": it
                // names BUILD QUEUE unless the list is clipped.
                assert!(
                    text.lines().any(|l| l.starts_with(" ↓")
                        && (l.contains(" card below: ") || l.contains(" cards below: "))
                        && (l.contains("BUILD QUEUE") || l.trim_end().ends_with('…'))),
                    "{text}"
                );
                // The selected card as drawn: its rule down to the next rule or the ↓ line.
                let selected = |frame: &str| {
                    let lines: Vec<&str> = frame.lines().collect();
                    let rule = |l: &&str| l.contains(" ──");
                    let c = lines.iter().position(|l| l.starts_with('▌'))?;
                    let start = lines[..c].iter().rposition(rule)?;
                    let end = lines[c..]
                        .iter()
                        .position(|l| rule(l) || l.starts_with(" ↓"))
                        .map_or(lines.len() - 1, |i| c + i);
                    Some(lines[start..end].join("\n"))
                };
                let mut cards = vec![];
                for _ in 0..6 {
                    input::key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
                    cards.extend(selected(&render(&app, width, height).0));
                }
                let card = |title: &str| {
                    cards
                        .iter()
                        .find(|c| c.starts_with(&format!(" {title} ─")))
                        .unwrap_or_else(|| {
                            panic!("{width}x{height}/{failed}: {title} never selected: {cards:#?}")
                        })
                        .clone()
                };
                (card("SHIPPED"), card("BUILD QUEUE"))
            } else {
                if !failed {
                    assert!(
                        text.contains("4 observed merged") && text.contains("2 active observed"),
                        "{text}"
                    );
                } else {
                    assert_eq!(text.matches("Counts unavailable").count(), 2, "{text}");
                }
                (text.clone(), text.clone())
            };
            // Each card keeps its whole floor: status, sample cap, rows.
            assert!(build.contains("latest100-run sample"), "{build}");
            if failed {
                assert!(
                    shipped.contains("Counts unavailable")
                        && shipped.contains("unavailable; unavailable"),
                    "{shipped}"
                );
                assert!(
                    build.contains("Counts unavailable")
                        && build.contains("authentication required"),
                    "{build}"
                );
            } else {
                assert!(
                    shipped.contains("sampled; ") && shipped.contains("@alex · 2 PRs observed"),
                    "{shipped}"
                );
                assert!(
                    build.contains("sampled; ")
                        && build.contains("running (in_progress)")
                        && (build.contains("queued (queued)") || build.contains("+1 more run")),
                    "{build}"
                );
            }
            // A stale PR read and its cap stay in the card too.
            app.github.sources[0].error = Some("GitHub unavailable");
            app.github.sources[0].total = 100;
            // Row 5 is #42 (two asks, three checks before it): the PR card in view.
            app.row = 5;
            let (text, _) = render(&app, width, height);
            assert!(
                text.contains("stale") && text.contains("first 100; more may exist"),
                "{width}x{height}/{failed}: {text}"
            );
        }
    }
}

/// The line the cursor is on, as drawn.
fn selected_line(text: &str) -> &str {
    text.lines().find(|l| l.starts_with('▌')).unwrap_or("")
}

/// Startup: before the ledger only the seeded PR source is listed (its stop, or its PRs).
/// The first ledger snapshot opens on the first ask in either arrival order; later
/// snapshots keep the cursor's identity, a stop's included, through a reorder.
#[test]
fn the_first_ledger_snapshot_opens_on_the_first_ask_and_later_ones_keep_the_row() {
    let repos = vec!["trip-to-japan/trip".to_string()];
    for (prs_first, w, h) in [
        (false, 120, 40),
        (true, 120, 40),
        (false, 46, 30),
        (true, 46, 30),
    ] {
        let mut app = App::new(Data::default());
        app.github = github::Dashboard::new(&repos);
        render(&app, w, h);
        if prs_first {
            app.github = github::demo();
            render(&app, w, h);
        }
        // As client.rs does: the snapshot, then loaded.
        app.snapshot(frames::fixture().glance);
        app.fetch.loaded = true;
        let (text, _) = render(&app, w, h);
        let line = selected_line(&text);
        assert!(
            app.row == 0 && line.contains("tui-frames") && line.contains("BLOCKING"),
            "{prs_first} {w}x{h}: row {} {line:?}\n{text}",
            app.row
        );
        // Loaded: a reorder keeps the cursor on its ask.
        let mut next = app.data.glance.clone();
        next.needs_you.reverse();
        app.snapshot(next);
        let (text, _) = render(&app, w, h);
        assert!(selected_line(&text).contains("tui-frames"), "{text}");
        assert_eq!(app.row, 1, "{text}");
        if !prs_first {
            // The unread PR card's stop, once chosen, is kept like any row.
            app.row = 5;
            let (text, _) = render(&app, w, h);
            assert!(
                selected_line(&text).contains("CI counts unavailable"),
                "{text}"
            );
            let mut next = app.data.glance.clone();
            next.needs_you.reverse();
            app.snapshot(next);
            let (text, _) = render(&app, w, h);
            assert_eq!(app.row, 5, "{text}");
            assert!(
                selected_line(&text).contains("CI counts unavailable"),
                "{text}"
            );
        }
    }
}
