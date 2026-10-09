#[cfg(feature = "contract")]
#[test]
fn synthetic_checkin_cell() {
    let output = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/checkin_cell.py"
        ))
        .arg("--rust")
        .arg(env!("CARGO_BIN_EXE_taskr"))
        .env_remove("TASKR_TASK")
        .env_remove("TASKR_LAUNCH")
        .output()
        .expect("run synthetic check-in cell");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
