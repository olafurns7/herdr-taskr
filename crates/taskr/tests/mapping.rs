//! testdata/contract/mapping.tsv stays consistent with the Rust tests and surfaces (non-strict).

use std::process::Command;

#[test]
fn mapping_check_passes() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let out = Command::new("python3")
        .arg(format!("{root}/tools/contract/mapping_check.py"))
        .env("CARGO", env!("CARGO"))
        .env_remove("TASKR_MAPPING_STRICT")
        .output()
        .expect("python3 runs mapping_check.py");
    assert!(
        out.status.success(),
        "mapping_check failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
