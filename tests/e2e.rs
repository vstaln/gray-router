//! End-to-end over the real wire: mock tier upstreams, spawn the
//! sidecar, drive `provider/chat`, and assert which upstream each
//! classified POST lands on.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// A mock upstream: serves one POST /responses with canned SSE, reports
/// the received body back over the channel.
fn mock_upstream() -> (u16, mpsc::Receiver<Value>) {
    let (tx, rx) = mpsc::channel();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut buf = vec![0u8; 65536];
            let mut head = Vec::new();
            let n = loop {
                let Ok(n) = s.read(&mut buf) else { return };
                head.extend_from_slice(&buf[..n]);
                if head.windows(4).any(|w| w == b"\r\n\r\n") {
                    break n;
                }
            };
            let _ = n;
            let text = String::from_utf8_lossy(&head).to_string();
            let len = text
                .lines()
                .find(|l| l.to_lowercase().starts_with("content-length:"))
                .and_then(|l| l.split(':').nth(1)?.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let pos = head
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map(|i| i + 4)
                .unwrap_or(head.len());
            let mut body = head[pos..].to_vec();
            while body.len() < len {
                let Ok(n) = s.read(&mut buf) else { break };
                if n == 0 {
                    break;
                }
                body.extend_from_slice(&buf[..n]);
            }
            let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let _ = tx.send(v);
            let sse = "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\ndata: [DONE]\n\n";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                sse.len(),
                sse
            );
            let _ = s.write_all(resp.as_bytes());
            let _ = s.flush();
        }
    });
    (port, rx)
}

struct Sidecar {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next: u64,
}

impl Sidecar {
    fn spawn(home: &std::path::Path) -> Self {
        let bin = env!("CARGO_BIN_EXE_gray-router");
        let mut child = Command::new(bin)
            .env("GRAY_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        Self {
            stdin: child.stdin.take().unwrap(),
            stdout: BufReader::new(child.stdout.take().unwrap()),
            child,
            next: 1,
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        let req = json!({"id": id, "method": method, "params": params});
        writeln!(self.stdin, "{req}").unwrap();
        self.stdin.flush().unwrap();
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }
}

fn write_router_json(home: &std::path::Path, fast_port: u16, smart_port: u16) {
    std::fs::create_dir_all(home).unwrap();
    let cfg = json!({
        "tiers": {
            "fast":  {"base_url": format!("http://127.0.0.1:{fast_port}"), "model": "mock-fast",  "api_key": "k"},
            "smart": {"base_url": format!("http://127.0.0.1:{smart_port}"), "model": "mock-smart", "api_key": "k"}
        },
        "default_tier": "smart"
    });
    std::fs::write(
        home.join("router.json"),
        serde_json::to_string_pretty(&cfg).unwrap(),
    )
    .unwrap();
}

fn post(url: &str, token: &str, body: &Value) -> u16 {
    // Minimal HTTP/1.1 POST (the relay is a raw TCP server).
    let bytes = serde_json::to_vec(body).unwrap();
    let url = url::parse_test(url);
    let mut s = std::net::TcpStream::connect(&url).unwrap();
    let req = format!(
        "POST /relay/{token}/responses HTTP/1.1\r\nHost: {url}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        bytes.len()
    );
    s.write_all(req.as_bytes()).unwrap();
    s.write_all(&bytes).unwrap();
    s.flush().unwrap();
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    loop {
        match s.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
        }
    }
    let text = String::from_utf8_lossy(&out);
    text.split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0)
}

mod url {
    /// "http://127.0.0.1:PORT" → "127.0.0.1:PORT"
    pub fn parse_test(u: &str) -> String {
        u.trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap()
            .to_string()
    }
}

fn turn(sc: &mut Sidecar, prompt: &str) -> (u16, String) {
    let reply = sc.call(
        "provider/chat",
        json!({"provider": "router", "auth_method": "router-config", "model": "auto"}),
    );
    let relay = reply["result"]["relay_url"].as_str().unwrap().to_string();
    let token = reply["result"]["relay_token"].as_str().unwrap().to_string();
    let body = json!({
        "model": "auto",
        "stream": true,
        "input": [{"role": "user", "content": [{"type": "input_text", "text": prompt}]}]
    });
    (post(&relay, &token, &body), relay)
}

#[test]
fn auto_routes_trivial_to_fast_and_complex_to_smart() {
    let home = tempfile::tempdir().unwrap();
    let (fast_port, fast_rx) = mock_upstream();
    let (smart_port, smart_rx) = mock_upstream();
    write_router_json(home.path(), fast_port, smart_port);
    let mut sc = Sidecar::spawn(home.path());

    // Manifest + catalog sanity.
    let m = sc.call("plugin/manifest", json!({}));
    assert_eq!(m["result"]["name"], "router");
    assert_eq!(m["result"]["protocol"], "1.2");
    let models = sc.call(
        "provider/models",
        json!({"provider": "router", "auth_method": "router-config"}),
    );
    let ids: Vec<&str> = models["result"]["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"auto") && ids.contains(&"fast") && ids.contains(&"smart"));

    // Trivial prompt → fast upstream, with the upstream's model name.
    let (status, _) = turn(&mut sc, "hi");
    assert_eq!(status, 200);
    let got = fast_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(got["model"], "mock-fast");
    assert!(smart_rx.recv_timeout(Duration::from_millis(500)).is_err());

    // Complex prompt → smart upstream.
    let complex = format!(
        "refactor the concurrency architecture and implement the migration {}",
        "x".repeat(2000)
    );
    let (status, _) = turn(&mut sc, &complex);
    assert_eq!(status, 200);
    let got = smart_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(got["model"], "mock-smart");

    // /route pin: fast pins every subsequent turn.
    let r = sc.call(
        "command/run",
        json!({"name": "/route", "argv": ["fast"]}),
    );
    assert!(r["result"]["text"].as_str().unwrap().contains("fast"));
    let _ = turn(&mut sc, &complex);
    assert!(fast_rx.recv_timeout(Duration::from_secs(10)).is_ok());

    // plugin/shutdown is a bare notification: no reply, clean exit.
    writeln!(sc.stdin, r#"{{"method":"plugin/shutdown"}}"#).unwrap();
    sc.stdin.flush().unwrap();
    let status = sc.child.wait().unwrap();
    assert!(status.success());
}
