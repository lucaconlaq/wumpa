use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{Ipv4Addr, TcpListener, TcpStream},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

struct Client {
    child: Child,
    input: Option<ChildStdin>,
    output: mpsc::Receiver<String>,
    text: String,
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Client {
    fn start(config: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_wumpa"))
            .arg("--plain")
            .env("WUMPA_CLIENT_CONFIG", config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let (sender, output) = mpsc::channel();
        thread::spawn(move || {
            let mut bytes = [0; 1024];
            while let Ok(count) = stdout.read(&mut bytes) {
                if count == 0
                    || sender
                        .send(String::from_utf8_lossy(&bytes[..count]).into_owned())
                        .is_err()
                {
                    break;
                }
            }
        });
        Self {
            input: child.stdin.take(),
            child,
            output,
            text: String::new(),
        }
    }

    fn send(&mut self, text: &str) {
        self.input
            .as_mut()
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
    }

    fn expect(&mut self, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.text.contains(expected) {
            let chunk = self
                .output
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| {
                    panic!("waiting for {expected:?}: {error}; output: {}", self.text)
                });
            self.text.push_str(&chunk);
        }
    }

    fn finish(&mut self) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(Instant::now() < deadline, "plain client did not exit");
            thread::sleep(Duration::from_millis(10));
        }
        let mut errors = String::new();
        self.child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut errors)
            .unwrap();
        errors
    }
}

fn receive(listener: &TcpListener) -> (TcpStream, Value) {
    let (mut stream, _) = listener.accept().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut line = String::new();
    BufReader::new(&mut stream).read_line(&mut line).unwrap();
    (stream, serde_json::from_str(&line).unwrap())
}

#[test]
fn plain_clone_confirms_destination_displays_result_and_cancels_on_input_or_eof() {
    for mode in ["success", "failure", "cancel", "eof"] {
        let dir = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let config = dir.path().join("client.json");
        std::fs::write(&config, json!({"servers": [{"name": "test", "type": "local", "port": listener.local_addr().unwrap().port()}]}).to_string()).unwrap();
        let (received, clone_received) = mpsc::channel();
        let responder = thread::spawn(move || {
            let (mut stream, request) = receive(&listener);
            assert_eq!(request["action"], "list");
            writeln!(stream, "{}", json!({"repositories": ["ssh://host/app.git"], "repository_dir": "/projects", "checkouts": [{"url": "ssh://host/app.git"}], "error": null})).unwrap();
            drop(stream);
            let (mut stream, request) = receive(&listener);
            assert_eq!(request["action"], "clone");
            assert_eq!(request["version"], 2);
            assert_eq!(request["url"], "ssh://host/app.git");
            assert_eq!(request["folder_name"], "review");
            assert!(request["agent_socket"].is_null());
            received.send(()).unwrap();
            if matches!(mode, "cancel" | "eof") {
                assert_eq!(stream.read(&mut [0]).unwrap(), 0);
            } else {
                thread::sleep(Duration::from_millis(100));
                let error = if mode == "failure" {
                    json!("\u{1b}[31mdenied")
                } else {
                    Value::Null
                };
                writeln!(stream, "{}", json!({"repositories": ["ssh://host/app.git"], "repository_dir": "/projects", "checkouts": [{"url": "ssh://host/app.git", "checkout_path": "/projects/review"}], "error": error, "preflight": {"version": 2, "destination": "/projects/review"}})).unwrap();
            }
        });
        let mut client = Client::start(&config);
        client.send("1\n");
        client.expect("Saved — not cloned");
        if mode == "success" {
            client.send("c\n1\nreview\n");
        } else {
            client.send("a\nssh://host/app.git\nreview\n");
        }
        client.expect("Clone on the server? [y/N]:");
        assert!(client.text.contains("/projects/review"));
        assert!(
            clone_received.try_recv().is_err(),
            "must not clone before confirmation"
        );
        client.text.clear();
        client.send("y\n");
        client.expect("Cloning…");
        clone_received.recv_timeout(Duration::from_secs(5)).unwrap();
        match mode {
            "success" => {
                client.expect("Cloned to");
                client.send("b\nq\n");
            }
            "failure" => {
                client.expect("[a] Clone repository");
                responder.join().unwrap();
                client.send("b\nq\n");
                let errors = client.finish();
                assert!(errors.contains("denied"));
                assert!(!errors.contains('\x1b'));
                continue;
            }
            "cancel" => {
                client.send("c\n");
                client.expect("cancellation requested");
                client.send("b\nq\n");
            }
            "eof" => {
                drop(client.input.take());
            }
            _ => unreachable!(),
        }
        assert!(client.finish().is_empty());
        responder.join().unwrap();
    }
}
