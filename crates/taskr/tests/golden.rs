fn root() -> &'static std::path::Path {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
}

#[test]
fn golden_child_context() {
    let mut command = std::process::Command::new("python3");
    command
        .arg(root().join("crates/taskr/tests/hub_child_context.py"))
        .arg("--expect")
        .arg(root().join("testdata/contract/golden"))
        .arg("--rust")
        .arg(env!("CARGO_BIN_EXE_taskr"));
    #[cfg(feature = "contract")]
    command.arg("--contract").arg(env!("CARGO_BIN_EXE_taskr"));
    let status = command
        .current_dir(root())
        .status()
        .expect("python3 is required for the golden contract tests");
    assert!(status.success(), "CLI context golden failed: {status}");
}

#[test]
#[cfg_attr(not(feature = "contract"), ignore = "needs --features contract")]
fn golden_contract() {
    let status = std::process::Command::new("python3")
        .arg(root().join("tools/contract/cells.py"))
        .arg("--expect")
        .arg(root().join("testdata/contract/golden"))
        .arg("--rust")
        .arg(env!("CARGO_BIN_EXE_taskr"))
        .current_dir(root())
        .status()
        .expect("python3 is required for the golden contract tests");
    assert!(status.success(), "golden contract checks failed: {status}");
}
