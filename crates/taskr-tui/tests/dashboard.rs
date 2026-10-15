//! Source freshness is independent even when the ledger has no usable snapshot.
use ratatui::{
    Terminal,
    backend::TestBackend,
    buffer::Buffer,
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
};
use taskr_tui::{App, Fetch, Screen, draw, frames, github, input, model::Data, prs};

fn configured_app(data: Data) -> App {
    let mut app = App::new(data);
    app.config.repos = vec!["example-org/app".into()];
    app.config.issues_repo = Some("example-org/app".into());
    app.config.health_repo = Some("example-org/app".into());
    app.config.panels[3..6].fill(true);
    app
}

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
            let mut app = configured_app(if state == 2 {
                frames::fixture()
            } else {
                Data::default()
            });
            app.mine = prs::demo(app.observed_at);
            app.fetch = Fetch {
                loaded: state != 1,
                error: (state > 0).then(|| "hub unreachable".into()),
                ..Fetch::default()
            };
            let (text, buffer) = render(&app, width, 30);
            assert!(
                text.contains("✗ #42") && text.contains("@demo-owner"),
                "{state}/{width}:\n{text}"
            );
            assert!(text.contains("6 observed"), "{text}");
            // The failing mark is the card's only red.
            let (y, line) = text
                .lines()
                .enumerate()
                .find(|(_, l)| l.contains("✗ #42"))
                .unwrap();
            let x = line.chars().position(|c| c == '✗').unwrap();
            assert_eq!(buffer[(x as u16, y as u16)].fg, app.theme.need);
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
    let mut app = configured_app(frames::fixture());
    app.fetch.loaded = true;
    app.mine = prs::demo(app.observed_at);
    app.mine.error = Some("GitHub unavailable");
    let (text, _) = render(&app, 46, 30);
    let header = text.lines().next().unwrap();
    assert!(
        header.contains("2 need you") && !header.contains("stale"),
        "{text}"
    );
    assert!(text.contains("stale") && text.contains("✗ #42"), "{text}");
    // No login: unavailable, never a guessed owner or an empty list.
    app.mine = prs::Mine::new(&["trip-to-japan/trip".into()]);
    app.mine.error = Some("GitHub authentication required");
    let (text, _) = render(&app, 46, 30);
    assert!(
        text.contains("unavailable: your GitHub") && !text.contains("0 observed"),
        "{text}"
    );
}

#[test]
fn filter_does_not_change_source_totals_and_plain_frames_keep_status_words() {
    let mut app = configured_app(frames::fixture());
    app.fetch.loaded = true;
    app.mine = prs::demo(app.observed_at);
    app.filter = "no matches".into();
    let (text, _) = render(&app, 46, 30);
    assert!(
        text.contains("6 observed") && text.contains("2 need you"),
        "{text}"
    );
    app.filter.clear();
    app.ascii = true;
    app.theme = taskr_tui::theme::PLAIN;
    let (text, buffer) = render(&app, 46, 30);
    assert!(text.is_ascii(), "{text}");
    assert!(
        text.contains("x #42") && text.contains("~ #41") && text.contains("v2 x2 ~0"),
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
    let mut app = configured_app(frames::fixture());
    app.fetch.loaded = true;
    app.mine = prs::demo(app.observed_at);
    app.mine.sample.found = 45;
    app.config.panels = [
        false, false, false, true, false, false, false, false, false, false, false,
    ];
    let (text, _) = render(&app, 46, 30);
    assert!(text.contains("first 30 of 45; more may exist"), "{text}");
    assert!(
        text.contains("◷12+?") && text.contains("1 not readable"),
        "{text}"
    );
}

#[test]
fn prs_use_empty_ledger_space_without_hiding_truth_placeholder() {
    for state in 0..3 {
        let mut app = configured_app(if state == 2 {
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
        app.mine = prs::demo(app.observed_at);
        let pr = app.mine.prs[0].clone();
        app.mine.prs = (1..=12)
            .map(|number| {
                let mut p = pr.clone();
                p.number = number;
                p
            })
            .collect();
        app.mine.sample.prs = app.mine.prs.clone();
        for (width, height) in [(46, 30), (120, 40)] {
            let (text, _) = render(&app, width, height);
            assert!(text.matches("✗ #").count() > 3, "{state}: {text}");
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
    let mut app = configured_app(Data::default());
    app.fetch.loaded = true;
    app.mine = prs::demo(app.observed_at);
    app.theme = taskr_tui::theme::LIGHT;
    let (text, buffer) = render(&app, 120, 40);
    assert!(
        text.contains("approved") && text.contains("changes"),
        "{text}"
    );
    for line in text.lines().filter(|l| l.contains(" · unres ")) {
        assert!(!line.trim_end_matches([' ', '│']).ends_with('·'), "{line}");
    }
    let (y, line) = text
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains("6 observed ·"))
        .unwrap();
    let x = line[..line.find("6 observed ·").unwrap()].chars().count();
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
            let mut app = configured_app(if state == 2 {
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
        let mut app = configured_app(Data::default());
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
    let mut app = configured_app(Data::default());
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
    let mut app = configured_app(Data::default());
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
            let mut app = configured_app(frames::fixture());
            app.fetch.loaded = true;
            app.mine = prs::demo(app.observed_at);
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
                text.contains("MY PRS") && text.contains("6 observed") && text.contains("✗ #42"),
                "{width}x{height}/{failed}: {text}"
            );
            if height == 20 {
                assert!(
                    !text.contains("BUILD QUEUE"),
                    "later kind displaced priority panel: {text}"
                );
            }
            // No PR silently dropped: each is drawn or counted on the card's more line.
            let missing = ["✗ #42", "◷ #41", "◷ #39", "? #37", "✓ #40", "□ #38"]
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
            let start = lines.iter().position(|l| l.contains("MY PRS")).unwrap();
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
            app.mine.error = Some("GitHub unavailable");
            app.mine.sample.found = 45;
            // Row 5 is #42 (two asks, three checks before it): the PR card in view.
            app.row = 5;
            let (text, _) = render(&app, width, height);
            assert!(
                text.contains("stale") && text.contains("first 30 of 45; more may exist"),
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
        let mut app = configured_app(Data::default());
        app.mine = prs::Mine::new(&repos);
        render(&app, w, h);
        if prs_first {
            app.mine = prs::demo(app.observed_at);
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
                selected_line(&text).contains("reading your GitHub login"),
                "{text}"
            );
            let mut next = app.data.glance.clone();
            next.needs_you.reverse();
            app.snapshot(next);
            let (text, _) = render(&app, w, h);
            assert_eq!(app.row, 5, "{text}");
            assert!(
                selected_line(&text).contains("reading your GitHub login"),
                "{text}"
            );
        }
    }
}

/// My PRs alone: wide rows carry their facts on one line, narrow rows on a second; Detail
/// says every field; no frame ever shows a required check green.
#[test]
fn my_prs_rows_and_detail_say_every_fact_and_never_a_required_green() {
    let app = |row: usize| {
        let mut app = configured_app(frames::fixture());
        app.fetch.loaded = true;
        app.mine = prs::demo(app.observed_at);
        app.config.panels = [
            false, false, false, true, false, false, false, false, false, false, false,
        ];
        app.row = row;
        app
    };
    let (text, _) = render(&app(0), 120, 40);
    let row = text.lines().find(|l| l.contains("✗ #42")).unwrap();
    assert!(
        row.contains("Keep pending action labels readable")
            && row.contains("✓2 ✗2 ◷0 · req ✗ · changes · behind · unres 3"),
        "{row}"
    );
    assert!(
        text.contains("@demo-owner")
            && text.contains("6 observed · demo-org/demo")
            && text.contains("1 not readable")
            && text.contains("? not known · N+? at least N"),
        "{text}"
    );
    let (text, _) = render(&app(0), 46, 30);
    let lines: Vec<&str> = text.lines().collect();
    let i = lines.iter().position(|l| l.contains("✗ #42")).unwrap();
    assert!(lines[i + 1].contains("✓2 ✗2 ◷0 · req ✗"), "{text}");
    let detail = |row: usize| {
        let mut app = app(row);
        input::key(
            &mut app,
            KeyEvent::new(KeyCode::Char('I'), KeyModifiers::NONE),
        );
        render(&app, 120, 60).0
    };
    // Sorted: #42 failing, #41 and #39 pending, #37 unknown, #40 passing, #38 draft.
    let text = detail(0);
    for want in [
        "PR #42 · demo-org/demo",
        "✗ e2e-web · required",
        "✗ lint · not required",
        "observed required: 1 passed, 1 failed",
        "complete required set: unknown",
        "merge state: BEHIND",
        "review: CHANGES_REQUESTED",
        "threads: 3 unresolved of 4 (all read)",
        "src/view.rs:142 · current",
        "docs/tui.md:12 · outdated",
        "GraphQL last cycle: cost 1 · 4989 remaining",
    ] {
        assert!(text.contains(want), "{want}:\n{text}");
    }
    let text = detail(2);
    assert!(
        text.contains("✓ 88+? ✗ 0+? ◷ 12+?") && text.contains("40 contexts not read"),
        "{text}"
    );
    let text = detail(3);
    assert!(
        text.contains("not readable") && text.contains("every field is ?"),
        "{text}"
    );
    let text = detail(4);
    assert!(
        text.contains("observed required: 1 passed, 0 failed")
            && text.contains("complete required set: unknown"),
        "{text}"
    );
    assert!(detail(5).contains("CHECKS · ? head moved"));
    for (w, h) in [(120, 40), (80, 30), (46, 30), (46, 20)] {
        for row in 0..6 {
            let (text, _) = render(&app(row), w, h);
            assert!(!text.contains("req ✓"), "{w}x{h}:\n{text}");
        }
    }
}

fn prs_only() -> App {
    let mut app = configured_app(frames::fixture());
    app.fetch.loaded = true;
    app.mine = prs::demo(app.observed_at);
    app.config.panels = [
        false, false, false, true, false, false, false, false, false, false, false,
    ];
    app
}

#[test]
fn wide_rows_keep_their_age_when_facts_are_long_or_stale() {
    for (width, stale) in [(100, false), (106, false), (120, true), (100, true)] {
        let mut app = prs_only();
        if stale {
            app.mine.error = Some("GitHub rate limited");
        }
        let (text, _) = render(&app, width, 40);
        // #42's facts and failing names and #39's lower bounds are the longest tails.
        for (pr, age) in [
            ("#42 ", "12m"),
            ("#41 ", "24m"),
            ("#40 ", "37m"),
            ("#39 ", "2h"),
            ("#38 ", "2h"),
        ] {
            let row = text
                .lines()
                .find(|l| l.contains(pr))
                .unwrap_or_else(|| panic!("{width}/{stale}: {text}"));
            assert!(
                row.trim_end().ends_with(&format!("  {age}")),
                "{width}/{stale}: {row}"
            );
        }
    }
    // Narrow rows keep the age on the title line, the facts on their own.
    let (text, _) = render(&prs_only(), 46, 30);
    let row = text.lines().find(|l| l.contains("#42 ")).unwrap();
    assert!(row.trim_end().ends_with(" 12m"), "{row}");
}

#[test]
fn card_legend_wraps_whole_below_the_wide_width() {
    for (width, height) in [(80, 30), (46, 30)] {
        let (text, _) = render(&prs_only(), width, height);
        for item in [
            "? not known",
            "N+? at least N, more unread",
            "req ✗ only from an observed required failure",
            "I every field",
        ] {
            assert!(text.contains(item), "{width}x{height}: {item}: {text}");
        }
        assert!(!text.contains("requir…"), "{text}");
    }
    // Wide, the legend stays one line.
    let (text, _) = render(&prs_only(), 120, 40);
    assert!(
        text.lines()
            .any(|l| l.contains("? not known · N+? at least N")
                && l.contains("req ✗ only from an observed required failure · I every field")),
        "{text}"
    );
}

#[test]
fn help_reaches_the_my_prs_legend_at_every_size() {
    for (width, height) in [(120, 40), (100, 30), (80, 30), (46, 30), (46, 20)] {
        let mut app = prs_only();
        app.screen = Screen::Help;
        let mut seen = String::new();
        for scroll in 0..120 {
            app.scroll = scroll;
            seen.push_str(&render(&app, width, height).0);
        }
        for want in [
            "My PRs",
            "failing",
            "pending",
            "not known",
            "checks passing",
            "draft",
            "N+? at least N, more unread.",
            "req ✗ only when a check GitHub marks",
            "never shown.",
            // The last campaign marks, which the 30-row wide help used to cut.
            "recommended",
        ] {
            assert!(seen.contains(want), "{width}x{height}: {want}");
        }
        assert!(!seen.contains("req ✓"), "{width}x{height}");
    }
    // The wide help says it scrolls when it is taller than the modal.
    let mut app = prs_only();
    app.screen = Screen::Help;
    let (text, _) = render(&app, 120, 40);
    assert!(text.contains("Keys · dashboard · 1-26/"), "{text}");
}
