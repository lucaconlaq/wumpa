//! Attachment discovery must fail closed against older or invalid servers.

#![cfg(unix)]

use std::{
    io::{BufRead, BufReader, Write},
    net::{Ipv4Addr, TcpListener},
    process::Command,
    thread,
};

#[test]
fn attachment_rejects_missing_and_invalid_discovery_without_a_picker() {
    for (metadata, expected) in [
        ("", "Server lacks attachment discovery"),
        (",\"control_socket\":\"relative.sock\"", "absolute"),
    ] {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(&mut stream).read_line(&mut request).unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&request).unwrap()["action"],
                "list"
            );
            writeln!(stream, "{{\"repositories\":[],\"error\":null{metadata}}}").unwrap();
        });
        let output = Command::new(env!("CARGO_BIN_EXE_wumpa"))
            .args([
                "agent-attach",
                "--port",
                &port.to_string(),
                "--session",
                &"a".repeat(32),
            ])
            .output()
            .unwrap();
        server.join().unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains(expected), "{error}");
    }
}
