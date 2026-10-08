//! Writes every frame as text and truecolor ANSI: `cargo run -p taskr-tui --example frames -- DIR`.
//! `tools/png.py DIR` then paints the PNGs and the contact sheet.

use std::{env, fs, path::PathBuf};

use taskr_tui::frames;

fn main() -> std::io::Result<()> {
    let dir = PathBuf::from(env::args().nth(1).unwrap_or_else(|| "frames".into()));
    fs::create_dir_all(&dir)?;
    for spec in frames::all() {
        let buf = frames::render(&spec);
        fs::write(dir.join(format!("{}.txt", spec.name)), frames::text(&buf))?;
        fs::write(dir.join(format!("{}.ansi", spec.name)), frames::ansi(&buf))?;
    }
    Ok(())
}
