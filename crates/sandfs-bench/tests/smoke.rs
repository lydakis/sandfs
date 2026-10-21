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
    assert!(stdout.contains("| target | 8 attempts, wall |"), "{stdout}");
    assert!(stdout.contains("\n| sandfs | "), "{stdout}");
}

/// A command in a copy of a folder: the copy, never the folder, gets the changes, each
/// column has its own cache folder, and read-only files and file times come across.
#[test]
fn a_command_runs_in_copies_of_a_folder() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("change.txt"), "x\n").unwrap();
    std::fs::write(dir.path().join("fixed.txt"), "y\n").unwrap();
    let then = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
    std::fs::File::options().write(true).open(dir.path().join("fixed.txt")).unwrap().set_modified(then).unwrap();
    std::fs::set_permissions(dir.path().join("fixed.txt"), std::fs::Permissions::from_mode(0o444)).unwrap();
    // Each run notes what it found in its copy, outside the copies.
    let log = tempfile::NamedTempFile::new().unwrap();
    // macOS pads `wc -l` with spaces and its `stat` takes `-f %m` for the modified time.
    let command = format!(
        "echo \"$(wc -l < change.txt | tr -d ' ') $(stat -c %Y fixed.txt 2>/dev/null || stat -f %m fixed.txt) \
         $(basename \"$XDG_CACHE_HOME\") $PWD\" >> {}",
        log.path().display()
    );
    let scratch = tempfile::tempdir().unwrap();
    let bench = |change: &str, parallel: &str| {
        Command::new(env!("CARGO_BIN_EXE_sandfs-bench"))
            .args(["--only", "build", "--targets", "host", "--iters", "2", "--build-parallel", parallel, "--scratch"])
            .arg(scratch.path())
            .arg("--build-dir")
            .arg(dir.path())
            .args(["--build-command", &command, "--build-change", change])
            .output()
            .unwrap()
    };

    let output = bench("change.txt", "1");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "stdout:\n{stdout}\nstderr:\n{}", String::from_utf8_lossy(&output.stderr));
    assert!(stdout.contains("| run again |"), "{stdout}");
    let runs: Vec<Vec<String>> = std::fs::read_to_string(log.path())
        .unwrap()
        .lines()
        .map(|line| line.split(' ').map(str::to_owned).collect())
        .collect();
    let lines: Vec<&str> = runs.iter().map(|run| run[0].as_str()).collect();
    assert_eq!(lines, ["1", "2", "3"], "a blank line before each run after the first");
    assert!(runs.iter().all(|run| run[1] == "1600000000" && run[2] == "cache-host-0"), "{runs:?}");
    assert!(runs.iter().all(|run| !run[3].starts_with(dir.path().to_str().unwrap())), "{runs:?}");
    assert_eq!(std::fs::read_to_string(dir.path().join("change.txt")).unwrap(), "x\n", "the folder is never changed");

    // Two copies at once: each copy and each run gets different bytes.
    std::fs::write(log.path(), "").unwrap();
    let output = bench("change.txt", "2");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "stdout:\n{stdout}\nstderr:\n{}", String::from_utf8_lossy(&output.stderr));
    assert!(stdout.contains("| | host folders |") && stdout.contains("2 at once"), "{stdout}");
    let mut copies: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    let mut caches: std::collections::BTreeSet<(String, String)> = Default::default();
    for line in std::fs::read_to_string(log.path()).unwrap().lines() {
        let run: Vec<&str> = line.split(' ').collect();
        copies.entry(run[3].to_owned()).or_default().push(run[0].to_owned());
        caches.insert((run[3].to_owned(), run[2].to_owned()));
    }
    // A cache folder of its own per copy, from the first run on.
    let names: std::collections::BTreeSet<&str> = caches.iter().map(|(_, cache)| cache.as_str()).collect();
    assert_eq!(names, ["cache-host-0", "cache-host-1"].into(), "{caches:?}");
    assert_eq!(caches.len(), 2, "{caches:?}");
    let mut lines: Vec<Vec<String>> = copies.into_values().collect();
    lines.sort();
    assert_eq!(lines, [["1", "2", "4"], ["1", "3", "5"]]);

    // A file outside the folder is refused before anything runs.
    let outside = dir.path().join("../outside.txt");
    std::fs::write(&outside, "z\n").unwrap();
    let output = bench(outside.to_str().unwrap(), "1");
    let _ = std::fs::remove_file(&outside);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("is no file in"));
}
