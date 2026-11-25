use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    let missing_config =
        std::env::temp_dir().join(format!("brewmble-fleet-test-{}.yaml", std::process::id()));
    assert!(!missing_config.exists());
    Command::new(env!("CARGO_BIN_EXE_brewmble"))
        .arg("--config")
        .arg(missing_config)
        .args(args)
        .env("BREWMBLE_NATIVE_PROGRESS", "1")
        .env_remove("BREWMBLE_TIMEOUT")
        .output()
        .unwrap()
}

#[test]
fn empty_fleet_produces_only_json() {
    for args in [
        &["status", "--json"][..],
        &["node", "--full-upgrade", "--dry-run", "--json"],
        &["node", "--reboot", "--json"],
        &["version", "--json"],
    ] {
        let output = run(args);
        assert_eq!(output.status.code(), Some(0), "{args:?}");
        assert_eq!(output.stdout, b"[]\n", "{args:?}");
        assert!(output.stderr.is_empty(), "{args:?}");
    }
}

#[test]
fn target_resolution_and_argument_errors_use_distinct_exit_codes() {
    let output = run(&["node", "--reboot", "--json", "unknown"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Node 'unknown' not found"));

    let output = run(&["node", "--list", "--json"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
}
