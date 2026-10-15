//! Exercise actual config loading, panel toggles and refreshes with gh isolated on PATH.
#[test]
fn unset_scopes_make_no_gh_calls_and_configured_scopes_are_used() {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            include_str!("repo_scopes.py"),
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
