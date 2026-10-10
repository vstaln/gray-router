//! `gray-router`: a protocol-1.2 provider sidecar for Gray — tiered model
//! routing over the relay surface.
//!
//! Wire (NDJSON over stdio, host ids are opaque):
//! - `plugin/manifest` → manifest + the `router` provider decl.
//! - `provider/models` → catalog: `auto` + one row per configured tier.
//! - `provider/chat` → one proxied turn (parks the row, hands back the
//!   per-turn relay URL + bearer the host's Responses POST uses).
//! - `provider/auth/*` → config-file probe; no credential dance (each tier
//!   carries its own key reference).
//! - `provider/usage` → `{available:false}`.
//! - `command/run` → `/route` mode/status/classify/setup.
//! - `prompt/context` → one status line while configured.
//! - `plugin/shutdown` → clean exit. `host/*` replies (string ids, no
//!   method) are ignored. Unknown methods are protocol errors — provider
//!   sidecars fail loudly, never hang a turn.

use gray_plugin::{ProviderRefreshRequest, ProviderRevokeRequest, ProviderRpcError};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};

use gray_router::{catalog, config, manifest, relay};

#[derive(Deserialize)]
struct Request {
    /// Notifications (`event/notify`, `plugin/shutdown`) carry no id.
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

#[derive(Serialize)]
struct Response {
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Value>,
}

type Relays = relay::Intents;

fn main() -> anyhow::Result<()> {
    // `gray plugin install`/`plugin list` registers sidecars by running
    // `<bin> manifest`; `sidecar` is accepted explicitly for parity with
    // manifests that declare sidecar_args.
    match std::env::args().nth(1).as_deref() {
        Some("-h") | Some("--help") | Some("help") => {
            println!(
                "gray-router — tiered model router provider for gray\n\n\
                 usage:\n  gray-router            run the sidecar (stdio JSON lines)\n  \
                 gray-router sidecar    same\n  gray-router manifest   print the plugin manifest\n  \
                 gray-router catalog    print the provider model catalog\n  \
                 gray-router setup      write a template ~/.gray/router.json"
            );
            return Ok(());
        }
        Some("manifest") => {
            println!("{}", serde_json::to_string(&manifest::manifest())?);
            return Ok(());
        }
        Some("setup") => {
            println!("{}", gray_router::setup::setup(true));
            return Ok(());
        }
        Some("catalog") => {
            let cfg = config::RouterConfig::load().ok();
            println!(
                "{}",
                serde_json::to_string_pretty(&catalog::catalog(cfg.as_ref()))?
            );
            return Ok(());
        }
        _ => {}
    }
    sidecar()
}

fn sidecar() -> anyhow::Result<()> {
    let relays: Relays = Arc::new(Mutex::new(HashMap::new()));
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let request: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(_) => continue,
        };
        // `plugin/shutdown` is a bare notification: exit, no reply.
        if request.method == "plugin/shutdown" {
            return Ok(());
        }
        // Other notifications and replies to sidecar-originated `host/*`
        // requests (string id, no method) need no answer.
        let Some(id) = request.id.clone() else {
            continue;
        };
        if request.method.is_empty() {
            continue;
        }
        let outcome = handle(&relays, &request);
        let response = match outcome {
            Ok(result) => Response {
                id,
                result: Some(result),
                error: None,
            },
            Err(error) => Response {
                id,
                result: None,
                error: Some(error_value(error)),
            },
        };
        writeln!(stdout, "{}", serde_json::to_string(&response)?)?;
        stdout.flush()?;
    }
    Ok(())
}

fn handle(relays: &Relays, request: &Request) -> Result<Value, ProviderRpcError> {
    let params = request.params.clone().unwrap_or_else(|| json!({}));
    match request.method.as_str() {
        "plugin/manifest" => serde_json::to_value(manifest::manifest())
            .map_err(|e| ProviderRpcError::Protocol(e.to_string())),
        "plugin/tools" => Ok(json!({"tools": []})),
        "provider/auth/start" => {
            ensure_provider(&param_str(&params, "provider"), &param_str(&params, "auth_method"))?;
            // External login: router.json IS the credential — connect just
            // checks it exists and names a fix when it doesn't.
            match config::RouterConfig::load() {
                Ok(_) => Ok(json!({"status": "authenticated"})),
                Err(e) => Err(ProviderRpcError::Unavailable(format!("{e:#}"))),
            }
        }
        "provider/auth/poll" => Err(ProviderRpcError::Protocol(
            "config-file auth has no pollable operation".into(),
        )),
        "provider/auth/cancel" => Ok(json!({})),
        "provider/auth/refresh" => {
            let req: ProviderRefreshRequest =
                serde_json::from_value(params).map_err(|_| invalid("provider refresh request"))?;
            ensure_provider(&req.provider, &req.auth_method)?;
            Ok(json!({"status": "unsupported"}))
        }
        "provider/auth/revoke" => {
            let req: ProviderRevokeRequest =
                serde_json::from_value(params).map_err(|_| invalid("provider revoke request"))?;
            ensure_provider(&req.provider, &req.auth_method)?;
            Ok(json!({"status": "unsupported"}))
        }
        "provider/models" => {
            ensure_provider(&param_str(&params, "provider"), &param_str(&params, "auth_method"))?;
            let cfg = config::RouterConfig::load().ok();
            serde_json::to_value(catalog::catalog(cfg.as_ref()))
                .map_err(|e| ProviderRpcError::Protocol(e.to_string()))
        }
        "provider/chat" => chat_turn(relays, &params),
        "provider/usage" => Ok(json!({"available": false})),
        "prompt/context" => Ok(manifest::prompt_context()),
        "command/run" => {
            let name = param_str(&params, "name");
            let argv: Vec<String> = params
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            if name == manifest::ROUTE_COMMAND {
                manifest::run_command(&argv)
                    .ok_or_else(|| ProviderRpcError::Protocol("unknown command".into()))
            } else {
                Err(ProviderRpcError::Protocol("unknown command".into()))
            }
        }
        _ => Err(ProviderRpcError::Protocol("unknown method".into())),
    }
}

/// One proxied turn: park the row, open the loopback relay, hand the host
/// its URL + per-turn bearer. The admitted POST classifies, rewrites and
/// forwards — this method only ever names where to POST.
fn chat_turn(relays: &Relays, params: &Value) -> Result<Value, ProviderRpcError> {
    ensure_provider(&param_str(params, "provider"), &param_str(params, "auth_method"))?;
    // Fail before opening anything when there is nothing to route to.
    let cfg = config::RouterConfig::load()
        .map_err(|e| ProviderRpcError::Unavailable(format!("{e:#}")))?;
    if cfg.tiers.is_empty() {
        return Err(ProviderRpcError::Unavailable(format!(
            "router: no tiers configured in {}",
            config::config_path().map(|p| p.display().to_string()).unwrap_or_default()
        )));
    }
    let row = param_str(params, "model");
    let row = if row.is_empty() { config::AUTO_ROW.to_string() } else { row };
    let bearer = format!("router-{}", hex_id());
    relays
        .lock()
        .map(|mut r| {
            r.insert(bearer.clone(), relay::RelayIntent { row });
        })
        .ok();
    let (port, _handle) =
        relay::start_turn_server(relays.clone(), bearer.clone())
            .map_err(ProviderRpcError::Unavailable)?;
    std::mem::forget(_handle);
    Ok(json!({
        "relay_url": format!("http://127.0.0.1:{port}/relay/{bearer}/responses"),
        "relay_token": bearer,
    }))
}

fn param_str(params: &Value, key: &str) -> String {
    params
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn hex_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{:08x}{:08x}", t, std::process::id())
}

fn ensure_provider(provider: &str, auth_method: &str) -> Result<(), ProviderRpcError> {
    if provider == manifest::PROVIDER_ID && auth_method == manifest::AUTH_METHOD_ID {
        Ok(())
    } else {
        Err(ProviderRpcError::Protocol(
            "unknown provider or auth method".into(),
        ))
    }
}

fn invalid(message: &'static str) -> ProviderRpcError {
    ProviderRpcError::Protocol(message.into())
}

fn error_value(error: ProviderRpcError) -> Value {
    match error {
        ProviderRpcError::Rpc(failure) => json!({
            "code": failure.code,
            "message": failure.message,
            "retryable": failure.retryable,
            "terminal": failure.terminal,
        }),
        ProviderRpcError::Protocol(message) => {
            json!({"code": "protocol", "message": message})
        }
        ProviderRpcError::Unavailable(message) => {
            json!({"code": "unavailable", "message": message})
        }
        ProviderRpcError::CapabilityMissing(message) => {
            json!({"code": "capability_missing", "message": message})
        }
    }
}
