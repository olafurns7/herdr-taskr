//! Every frame against its golden text file. `UPDATE_GOLDEN=1 cargo test -p taskr-tui`
//! rewrites them after a deliberate change to the look.

use std::{collections::BTreeSet, env, fs, path::PathBuf};

use taskr_tui::frames;

/// The spec's glyph set (plan §3, Glyphs), plus `…` and `—`, which §3 itself prescribes
/// for the free-text option and for "no open lanes", and Q2's `◇` and `★` for asks.
const GLYPHS: &str = "●○◐◓◑◒•·×✓✗‖▌❯↗⏎─│╭╮╰╯▁▂▃▄▅▆▇█░⊘◆◇★▲↳‹…—";

fn golden() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

#[test]
fn frames_match_their_golden_files() {
    let update = env::var_os("UPDATE_GOLDEN").is_some();
    let mut wrong = vec![];
    for spec in frames::all() {
        let path = golden().join(format!("{}.txt", spec.name));
        let text = frames::text(&frames::render(&spec));
        if update {
            fs::write(&path, &text).expect("write a golden file");
        } else if fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
            wrong.push(format!("--- {}\n{text}", spec.name));
        }
    }
    assert!(
        wrong.is_empty(),
        "frames differ from tests/golden (UPDATE_GOLDEN=1 rewrites):\n{}",
        wrong.join("\n")
    );
}

#[test]
fn every_golden_file_has_a_frame() {
    let names: BTreeSet<String> = frames::all()
        .into_iter()
        .map(|s| format!("{}.txt", s.name))
        .collect();
    let files: BTreeSet<String> = fs::read_dir(golden())
        .expect("tests/golden")
        .map(|e| {
            e.expect("an entry")
                .file_name()
                .into_string()
                .expect("a name")
        })
        .collect();
    assert_eq!(names, files);
}

/// The fixture's own text is ASCII, so this covers everything the view draws. Live
/// ledger text can hold anything.
#[test]
fn glyphs_come_from_the_spec_set() {
    for spec in frames::all() {
        let text = frames::text(&frames::render(&spec));
        let odd: BTreeSet<char> = text
            .chars()
            .filter(|c| !c.is_ascii() && !GLYPHS.contains(*c))
            .collect();
        assert!(odd.is_empty(), "{}: {odd:?}", spec.name);
    }
}

/// The owner's 46-column pane: no more than one blank row while a second line is folded.
/// Frames with no list to fold.
const STATES: [&str; 3] = ["-loading", "-nodata", "-empty"];

#[test]
fn the_narrow_glance_wastes_no_rows() {
    for spec in frames::all()
        .iter()
        .filter(|s| s.name.starts_with("glance-46x") && !STATES.iter().any(|x| s.name.ends_with(x)))
    {
        let text = frames::text(&frames::render(spec));
        let blank = text.lines().filter(|l| l.trim().is_empty()).count();
        assert!(blank <= 1, "{}: {blank} blank rows\n{text}", spec.name);
    }
}
