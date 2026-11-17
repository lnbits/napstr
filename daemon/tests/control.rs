#[cfg(unix)]
#[test]
fn pairing_command_uses_the_running_service_and_writes_a_private_qr() {
    use napstr_remote_protocol::{PairingTicket, PROTOCOL_VERSION};
    use std::{
        fs,
        io::{Read, Write},
        os::unix::{fs::PermissionsExt, net::UnixListener},
        process::Command,
        thread,
    };
    let dir = tempfile::tempdir().unwrap();
    let listener = UnixListener::bind(dir.path().join("control.sock")).unwrap();
    let endpoint = iroh::SecretKey::generate().public().to_string();
    let ticket = PairingTicket {
        version: PROTOCOL_VERSION,
        endpoint_id: endpoint.clone(),
        endpoint_addr: String::new(),
        token: "a".repeat(64),
        expires_at: 2_000_000_000,
        desktop_name: "Napstr daemon".into(),
    }
    .to_uri()
    .unwrap();
    let expected_ticket = ticket.clone();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut size = [0; 4];
        socket.read_exact(&mut size).unwrap();
        let mut bytes = vec![0; u32::from_be_bytes(size) as usize];
        socket.read_exact(&mut bytes).unwrap();
        let request: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            request,
            serde_json::json!({"command":"pair", "read_only":true})
        );
        let response = serde_json::to_vec(&serde_json::json!({"result":"pairing", "ticket":ticket, "expiresAt":2_000_000_000u64, "endpointId":endpoint})).unwrap();
        socket
            .write_all(&(response.len() as u32).to_be_bytes())
            .unwrap();
        socket.write_all(&response).unwrap();
    });
    let svg = dir.path().join("pair.svg");
    let output = Command::new(env!("CARGO_BIN_EXE_napstrd"))
        .env_clear()
        .env("NAPSTR_DATA_DIR", dir.path())
        .args(["pair", "--read-only", "--json", "--qr-file"])
        .arg(&svg)
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["ticket"], expected_ticket);
    assert!(fs::read_to_string(&svg).unwrap().contains("<svg"));
    assert_eq!(
        fs::metadata(svg).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(!dir.path().join("daemon.lock").exists());
    assert!(!dir.path().join("iroh-identity").exists());
}
