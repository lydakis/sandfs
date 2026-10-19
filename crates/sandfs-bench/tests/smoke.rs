//! Runs every workload once at the smallest scale on every target available here, so
//! the suite keeps working as the crates change. Each workload checks its own result.

use std::process::Command;

#[test]
fn every_workload_runs_on_every_available_target() {
    let scratch = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_sandfs-bench"))
        .args(["--scale", "tiny", "--latency-us", "100", "--scratch"])
        .arg(scratch.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    for row in ["| small/read-cold |", "| deep/stat |", "| fork/changed |", "| changes |", "| lazy/forks |"] {
        assert!(stdout.contains(row), "{row} missing from:\n{stdout}");
    }
    assert!(stdout.contains("| host | sandfs |"), "{stdout}");
}
