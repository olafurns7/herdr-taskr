//! Exercise the actual SaveConfig event handler, with all reads isolated from the host.
#[test]
fn toggles_preserve_bad_or_unreadable_existing_config() {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            include_str!("config_preservation.py"),
            env!("CARGO_BIN_EXE_taskr-tui"),
        ])
        .output()
        .expect("python3 (also used by the repository's fixture tools)");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
