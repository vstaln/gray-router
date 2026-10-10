//! Per-turn loopback relay: the request gate in front of the tier
//! upstreams.
//!
//! `provider/chat` parks the selected catalog row and hands the host a
//! one-shot loopback URL + bearer. The admitted POST carries the host's
//! standard Responses body; the relay classifies it (`auto`), rewrites
//! `model` to the winning tier's upstream model, and forwards the body
//! verbatim to `tier.base_url + /responses` under the tier's own
//! credential. Upstream SSE streams straight back — the relay never
//! reframes, so usage, reasoning items and tool calls survive intact.
//!
//! `previous_response_id` is stripped: it pins a reply chain to the model
//! that made it, and the routed-to model is by definition a different one.
//!
//! The wire shape mirrors gray-devin-sub's admission relay: single
//! admission, `Content-Length` POST only, `: keepalive` SSE comments past
//! the header grace while upstream thinks.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::config::{RouterConfig, RouterState, Tier, AUTO_ROW};

/// Hold headers this long before committing to the keepalive stream (the
/// host's per-read timeout is longer; devin-sub uses the same budget).
const HEADER_GRACE: Duration = Duration::from_secs(20);
/// SSE comment cadence while waiting on upstream.
const KEEPALIVE: Duration = Duration::from_secs(15);
/// Poll cadence for client hang-up while waiting.
const PEER_POLL: Duration = Duration::from_millis(250);
/// Upstream connect/read bounds: a stalled upstream must die, not hang.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const READ_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_BODY: usize = 256 << 20;

/// What `provider/chat` parks per turn: the catalog row the host picked.
pub struct RelayIntent {
    pub row: String,
}

pub type Intents = Arc<Mutex<HashMap<String, RelayIntent>>>;

/// Start the per-turn relay server. Returns the loopback port; the
/// admitted POST does the whole proxied turn on a worker thread, and the
/// listener stays up to 400 anything past the single admission.
pub fn start_turn_server(
    intents: Intents,
    bearer: String,
) -> Result<(u16, std::thread::JoinHandle<()>), String> {
    let listener =
        TcpListener::bind("127.0.0.1:0").map_err(|e| format!("relay bind: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("relay addr: {e}"))?
        .port();
    let used = Arc::new(AtomicBool::new(false));
    let handle = std::thread::spawn(move || serve(listener, intents, bearer, used));
    Ok((port, handle))
}

fn serve(
    listener: TcpListener,
    intents: Intents,
    bearer: String,
    used: Arc<AtomicBool>,
) {
    for stream in listener.incoming() {
        let Ok(mut s) = stream else { continue };
        handle_conn(&mut s, &intents, &bearer, &used);
    }
}

/// One upstream→client message: status first, then body chunks.
enum Up {
    Head(u16),
    Chunk(Vec<u8>),
    Done,
    Err(String),
}

enum Wait {
    Msg(Up),
    Timeout,
    Died,
    Gone,
}

fn client_gone(s: &std::net::TcpStream) -> bool {
    if s.set_nonblocking(true).is_err() {
        return false;
    }
    let gone = match s.peek(&mut [0u8; 1]) {
        Ok(n) => n == 0,
        Err(e) => e.kind() != std::io::ErrorKind::WouldBlock,
    };
    let _ = s.set_nonblocking(false);
    gone
}

fn wait_up(
    rx: &mpsc::Receiver<Up>,
    s: &std::net::TcpStream,
    span: Duration,
) -> Wait {
    let end = Instant::now() + span;
    loop {
        let step = end.saturating_duration_since(Instant::now()).min(PEER_POLL);
        match rx.recv_timeout(step) {
            Ok(m) => return Wait::Msg(m),
            Err(mpsc::RecvTimeoutError::Disconnected) => return Wait::Died,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if client_gone(s) {
                    return Wait::Gone;
                }
                if Instant::now() >= end {
                    return Wait::Timeout;
                }
            }
        }
    }
}

fn handle_conn(
    s: &mut std::net::TcpStream,
    intents: &Intents,
    bearer: &str,
    used: &Arc<AtomicBool>,
) {
    let _ = s.set_read_timeout(Some(Duration::from_secs(330)));
    let mut buf = vec![0u8; 65536];
    let mut head = Vec::new();
    loop {
        let Ok(n) = s.read(&mut buf) else { return };
        if n == 0 {
            return;
        }
        head.extend_from_slice(&buf[..n]);
        if head.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if head.len() > 1_048_576 {
            return;
        }
    }
    let head_str = String::from_utf8_lossy(&head).into_owned();
    let mut lines = head_str.lines();
    let request_line = lines.next().unwrap_or("").to_string();
    let mut len = 0usize;
    let mut auth = String::new();
    for line in lines {
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let Some((name, v)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            len = v.trim().parse().unwrap_or(0);
        } else if name.eq_ignore_ascii_case("authorization") {
            auth = v.trim().to_string();
        }
    }
    let header_end = head
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(head.len());
    let mut body = head[header_end..].to_vec();
    while body.len() < len.min(MAX_BODY) {
        let Ok(n) = s.read(&mut buf) else { return };
        if n == 0 {
            break;
        }
        body.extend_from_slice(&buf[..n]);
    }
    let ok_path = request_line.starts_with("POST /relay/")
        && request_line.contains("/responses")
        && request_line.contains(bearer);
    let ok_bearer = auth == format!("Bearer {bearer}");
    if !ok_path || !ok_bearer {
        write_resp(s, 404, b"not found");
        return;
    }
    if len > MAX_BODY || body.len() < len {
        let body = json!({"type": "error", "error": {
            "type": "invalid_request_error",
            "message": format!("relay body incomplete: read {} of {len} bytes (limit {MAX_BODY})", body.len()),
        }});
        write_resp(
            s,
            if len > MAX_BODY { 413 } else { 400 },
            body.to_string().as_bytes(),
        );
        return;
    }
    if used.swap(true, Ordering::SeqCst) {
        let body = json!({"type": "error", "error": {
            "type": "invalid_request_error",
            "message": "admission consumed: this relay already served one turn",
        }});
        write_resp(s, 400, body.to_string().as_bytes());
        return;
    }

    // Admitted: route + proxy the turn on a worker; the connection either
    // answers within HEADER_GRACE or commits to a kept-alive SSE stream.
    let (tx, rx) = mpsc::channel::<Up>();
    {
        let intents = intents.clone();
        let bearer = bearer.to_string();
        std::thread::spawn(move || run_turn(&intents, &bearer, &body, &tx));
    }

    let mut headers_sent = false;
    loop {
        match wait_up(&rx, s, if headers_sent { KEEPALIVE } else { HEADER_GRACE }) {
            Wait::Gone | Wait::Died => return,
            Wait::Msg(Up::Head(status)) => {
                if !headers_sent {
                    if (200..300).contains(&status) {
                        if s.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
                        )
                        .and_then(|_| s.flush())
                        .is_err()
                        {
                            return;
                        }
                    }
                    headers_sent = true;
                    if !(200..300).contains(&status) {
                        // Upstream rejected: wait for its error body and
                        // relay it under the same status.
                        match drain_err(&rx) {
                            Some(err_body) => {
                                write_resp(s, status, &err_body);
                            }
                            None => write_resp(
                                s,
                                status,
                                br#"{"error":{"message":"upstream error"}}"#,
                            ),
                        }
                        return;
                    }
                } else if !(200..300).contains(&status) {
                    // Kept-alive stream already committed to 200: fold the
                    // upstream failure into a Responses error event.
                    let detail = drain_err_text(&rx);
                    write_failed(s, &format!("upstream {status}: {detail}"));
                    return;
                }
            }
            Wait::Msg(Up::Chunk(c)) => {
                if !headers_sent {
                    // Chunk before Head is a worker bug; ignore it.
                    continue;
                }
                if s.write_all(&c).and_then(|_| s.flush()).is_err() {
                    return;
                }
            }
            Wait::Msg(Up::Done) => return,
            Wait::Msg(Up::Err(detail)) => {
                if !headers_sent {
                    let body = json!({"type": "error", "error": {
                        "type": "server_error", "message": detail,
                    }});
                    write_resp(s, 502, body.to_string().as_bytes());
                } else {
                    write_failed(s, &detail);
                }
                return;
            }
            Wait::Timeout => {
                if !headers_sent {
                    // No upstream head inside the grace: commit to the
                    // close-delimited stream and keepalive until it lands.
                    if s.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
                    )
                    .and_then(|_| s.flush())
                    .is_err()
                    {
                        return;
                    }
                    headers_sent = true;
                } else if s
                    .write_all(b": keepalive\n\n")
                    .and_then(|_| s.flush())
                    .is_err()
                {
                    return;
                }
            }
        }
    }
}

/// After a non-2xx `Head`, collect the upstream error body bytes.
fn drain_err(rx: &mpsc::Receiver<Up>) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Up::Chunk(c)) => out.extend_from_slice(&c),
            _ => break,
        }
        if Instant::now() > deadline || out.len() > 1 << 20 {
            break;
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn drain_err_text(rx: &mpsc::Receiver<Up>) -> String {
    drain_err(rx)
        .map(|b| String::from_utf8_lossy(&b).chars().take(500).collect())
        .unwrap_or_else(|| "no upstream error body".to_string())
}

/// A `response.failed` SSE event + `[DONE]` on an open stream.
fn write_failed(s: &mut std::net::TcpStream, detail: &str) {
    let failed = json!({"type": "response.failed",
        "response": {"status": "failed",
            "error": {"code": "server_error", "message": detail}}});
    let _ = s.write_all(format!("data: {failed}\n\ndata: [DONE]\n\n").as_bytes());
    let _ = s.flush();
}

/// Route + proxy one turn. The upstream request runs to completion here;
/// `tx` delivers head → chunks → done.
fn run_turn(intents: &Intents, bearer: &str, raw: &[u8], tx: &mpsc::Sender<Up>) {
    let send = |m: Up| {
        let _ = tx.send(m);
    };
    let intent = intents
        .lock()
        .map(|mut m| m.remove(bearer))
        .ok()
        .flatten();
    let Some(intent) = intent else {
        send(Up::Err("relay intent expired".into()));
        return;
    };
    let body: Value = match serde_json::from_slice(raw) {
        Ok(b) => b,
        Err(e) => {
            send(Up::Err(format!("relay body is not JSON: {e}")));
            return;
        }
    };
    let cfg = match RouterConfig::load() {
        Ok(c) => c,
        Err(e) => {
            send(Up::Err(format!("{e:#}")));
            return;
        }
    };
    let mut state = RouterState::load();
    let (tier_name, reason) = resolve_tier(&body, &cfg, &state, &intent.row);
    let Some(tier_name) = tier_name else {
        send(Up::Err(format!(
            "router: no usable tier — check {} tiers",
            crate::config::config_path()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        )));
        return;
    };
    let tier = match cfg.tiers.get(&tier_name) {
        Some(t) => t.clone(),
        None => {
            send(Up::Err(format!("router: tier \"{tier_name}\" is not configured")));
            return;
        }
    };
    let Some(key) = tier.api_key() else {
        send(Up::Err(format!(
            "router: no API key for tier \"{tier_name}\" — set {} or api_key in router.json",
            tier.api_key_env.as_deref().unwrap_or("<api_key_env>")
        )));
        return;
    };
    state.record(&tier_name, &tier.model, &reason);
    if cfg.announce {
        say(&format!("router → {tier_name} ({}) — {reason}", tier.model));
    }

    // Rewrites: the upstream model wins, and the reply chain can't point
    // back at a response a different model made.
    let mut out = body.clone();
    out["model"] = json!(tier.model);
    if let Some(map) = out.as_object_mut() {
        map.remove("previous_response_id");
    }
    let Ok(bytes) = serde_json::to_vec(&out) else {
        send(Up::Err("router: cannot serialize forwarded body".into()));
        return;
    };
    proxy(&tier, &key, &bytes, &send);
}

/// Which upstream tier this request takes.
pub fn resolve_tier(
    body: &Value,
    cfg: &RouterConfig,
    state: &RouterState,
    row: &str,
) -> (Option<String>, String) {
    let row_is_tier = cfg.tiers.contains_key(row);
    match state.mode.as_str() {
        // Off: no classification; a tier row still acts as an alias,
        // `auto` falls back to the default.
        "off" => {
            if row_is_tier {
                (Some(row.to_string()), "off; tier row".into())
            } else {
                (cfg.fallback_tier(), "off; default tier".into())
            }
        }
        // A tier name pins every request regardless of the row.
        mode if cfg.tiers.contains_key(mode) => {
            (Some(mode.to_string()), format!("pinned /route {mode}"))
        }
        // Auto: an explicitly picked tier row wins; `auto` classifies.
        _ => {
            if row_is_tier {
                (Some(row.to_string()), "tier row".into())
            } else if row == AUTO_ROW {
                if cfg.classifier.kind == "llm" {
                    match llm_classify(body, cfg) {
                        Some(t) => return (Some(t), "llm classify".into()),
                        None => log_line("llm classify failed; heuristic fallback"),
                    }
                }
                match crate::classify::classify(body, cfg) {
                    Some((t, why)) => (Some(t), why),
                    None => (cfg.fallback_tier(), "auto; default tier".into()),
                }
            } else {
                (cfg.fallback_tier(), "auto; default tier".into())
            }
        }
    }
}

/// Optional LLM classification behind `classifier.kind = "llm"`: one tiny
/// non-streaming call on `classifier.tier`'s upstream. Any failure falls
/// back to the heuristic — classification must never kill a turn.
fn llm_classify(body: &Value, cfg: &RouterConfig) -> Option<String> {
    let tier = cfg.tiers.get(&cfg.classifier.tier)?;
    let key = tier.api_key()?;
    let prompt_text = crate::classify::last_user_text(body);
    let prompt_text: String = prompt_text.chars().take(2000).collect();
    let names = cfg.tier_names().join("|");
    let req = json!({
        "model": tier.model,
        "store": false,
        "input": [{"role": "user", "content": [{"type": "input_text",
            "text": format!(
                "Pick which model tier should handle the request below. \
                 Reply with exactly one word: {names}.\n\n{prompt_text}"
            )}]}],
    });
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(Duration::from_secs(15))
        .build();
    let resp = agent
        .post(&tier.responses_url())
        .set("Authorization", &format!("Bearer {key}"))
        .set("Content-Type", "application/json")
        .send_json(&req)
        .ok()?;
    let v: Value = resp.into_json().ok()?;
    let answer = extract_output_text(&v).to_lowercase();
    cfg.tier_names()
        .into_iter()
        .find(|n| answer.split(|c: char| !c.is_alphanumeric()).any(|w| w == n))
}

/// First `output_text` of a non-streaming Responses reply.
fn extract_output_text(v: &Value) -> String {
    v.get("output")
        .and_then(|o| o.as_array())
        .map(|items| {
            items
                .iter()
                .flat_map(|i| {
                    i.get("content")
                        .and_then(|c| c.as_array())
                        .cloned()
                        .unwrap_or_default()
                })
                .filter_map(|c| c.get("text").and_then(|t| t.as_str()).map(str::to_string))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

/// Forward the body upstream and stream the response back chunk by chunk.
fn proxy(tier: &Tier, key: &str, body: &[u8], send: &dyn Fn(Up)) {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(READ_TIMEOUT)
        .build();
    let mut req = agent
        .post(&tier.responses_url())
        .set("Authorization", &format!("Bearer {key}"))
        .set("Content-Type", "application/json");
    for (name, value) in &tier.headers {
        req = req.set(name, value);
    }
    match req.send_bytes(body) {
        Ok(resp) => {
            send(Up::Head(resp.status()));
            let mut reader = resp.into_reader();
            let mut buf = [0u8; 16384];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        send(Up::Done);
                        return;
                    }
                    Ok(n) => send(Up::Chunk(buf[..n].to_vec())),
                    Err(e) => {
                        send(Up::Err(format!("upstream read: {e}")));
                        return;
                    }
                }
            }
        }
        Err(ureq::Error::Status(status, resp)) => {
            send(Up::Head(status));
            let mut reader = resp.into_reader();
            let mut buf = [0u8; 16384];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        send(Up::Done);
                        return;
                    }
                    Ok(n) => send(Up::Chunk(buf[..n].to_vec())),
                    Err(_) => {
                        send(Up::Done);
                        return;
                    }
                }
            }
        }
        Err(e) => send(Up::Err(format!("upstream connect: {e}"))),
    }
}

/// `host/say` sidecar→host frame (string id; the reply is ignored).
fn say(text: &str) {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = format!("router-say-{}", N.fetch_add(1, Ordering::Relaxed));
    let frame = json!({"id": id, "method": "host/say", "params": {"text": text}});
    let _ = writeln!(std::io::stdout(), "{frame}");
    let _ = std::io::stdout().flush();
}

fn log_line(text: &str) {
    eprintln!("gray-router: {text}");
}

fn write_resp(s: &mut std::net::TcpStream, status: u16, body: &[u8]) {
    let reason = match status {
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = s.write_all(header.as_bytes());
    let _ = s.write_all(body);
    let _ = s.flush();
}
