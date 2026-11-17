use nostr_sdk::prelude::*;
use std::{fs, process::Command};

#[test]
fn cli_validates_runtime_configuration_and_keeps_the_key_private() {
    let help = Command::new(env!("CARGO_BIN_EXE_napstrd"))
        .env_clear()
        .arg("--help")
        .output()
        .unwrap();
    assert!(help.status.success());
    let missing = Command::new(env!("CARGO_BIN_EXE_napstrd"))
        .env_clear()
        .arg("--check")
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("NAPSTR_NSEC is required"));

    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    let state = dir.path().join("state");
    fs::create_dir(&music).unwrap();
    let keys = Keys::generate();
    let secret = keys.secret_key().to_bech32().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_napstrd"))
        .env_clear()
        .env("NAPSTR_NSEC", &secret)
        .env("NAPSTR_MUSIC_DIR", &music)
        .env("NAPSTR_DATA_DIR", &state)
        .arg("--check")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(&keys.public_key().to_bech32().unwrap()));
    assert!(!stdout.contains(&secret));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(&secret));
    assert_eq!(fs::read_dir(&music).unwrap().count(), 0);
    for entry in fs::read_dir(&state).unwrap() {
        assert!(!fs::read_to_string(entry.unwrap().path())
            .unwrap()
            .contains(&secret));
    }
}

#[cfg(unix)]
#[test]
fn sigterm_during_tor_startup_exits_and_tor_does_not_inherit_nsec() {
    use std::{
        os::unix::fs::PermissionsExt,
        thread,
        time::{Duration, Instant},
    };

    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    fs::create_dir(&music).unwrap();
    let tor = dir.path().join("fake-tor");
    fs::write(&tor, "#!/bin/sh\nif [ -n \"${NAPSTR_NSEC+x}\" ]; then\n  echo leaked > \"$TEST_TOR_MARKER\"\nelse\n  echo clean > \"$TEST_TOR_MARKER\"\nfi\nexec sleep 300\n").unwrap();
    fs::set_permissions(&tor, fs::Permissions::from_mode(0o700)).unwrap();
    let marker = dir.path().join("tor-environment");
    let log = fs::File::create(dir.path().join("stderr")).unwrap();
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_napstrd"))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env(
            "NAPSTR_NSEC",
            Keys::generate().secret_key().to_bech32().unwrap(),
        )
        .env("NAPSTR_MUSIC_DIR", music)
        .env("NAPSTR_DATA_DIR", dir.path().join("state"))
        .env("NAPSTR_TOR_PATH", tor)
        .env("TEST_TOR_MARKER", &marker)
        .stderr(log)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !marker.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    if !marker.exists() {
        let _ = daemon.kill();
        let _ = daemon.wait();
        panic!("daemon did not start the test Tor process");
    }
    let devices = Command::new(env!("CARGO_BIN_EXE_napstrd"))
        .env_clear()
        .env("NAPSTR_DATA_DIR", dir.path().join("state"))
        .args(["devices", "--json"])
        .output()
        .unwrap();
    assert!(
        devices.status.success(),
        "{}",
        String::from_utf8_lossy(&devices.stderr)
    );
    let devices: serde_json::Value = serde_json::from_slice(&devices.stdout).unwrap();
    assert_eq!(devices["devices"], serde_json::json!([]));
    assert_eq!(
        fs::metadata(dir.path().join("state/control.sock"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(Command::new("kill")
        .args(["-TERM", &daemon.id().to_string()])
        .status()
        .unwrap()
        .success());
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = daemon.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = daemon.kill();
            let _ = daemon.wait();
            panic!("daemon did not stop after SIGTERM");
        }
        thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success());
    assert_eq!(fs::read_to_string(marker).unwrap().trim(), "clean");
}
