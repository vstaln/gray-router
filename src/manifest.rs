//! Protocol-1.2 manifest + the `/route` command surface.

use gray_plugin::{AuthMethodDecl, ProviderDecl, ProviderTransportDecl, PROVIDER_CREDENTIALS};
use serde_json::{json, Value};

use crate::config::{self, RouterConfig, RouterState, AUTO_ROW};

pub const PLUGIN_NAME: &str = "router";
pub const PROVIDER_ID: &str = "router";
pub const AUTH_METHOD_ID: &str = "router-config";
pub const ROUTE_COMMAND: &str = "/route";

pub fn manifest() -> gray_plugin::Manifest {
    gray_plugin::Manifest {
        name: PLUGIN_NAME.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        tools: Vec::new(),
        commands: vec![ROUTE_COMMAND.to_string()],
        hooks: vec!["prompt/context".to_string()],
        protocol: Some("1.2".to_string()),
        subcommands: Vec::new(),
        capabilities: vec![
            PROVIDER_CREDENTIALS.to_string(),
            gray_plugin::capabilities::HOST_SAY.to_string(),
        ],
        providers: vec![provider()],
        provider_errors: Vec::new(),
    }
}

/// Router provider. `base_url` is a placeholder: every turn POSTs to the
/// per-turn loopback relay the sidecar hands out in `provider/chat`
/// (`relay_url`/`relay_token`), never to this URL.
pub fn provider() -> ProviderDecl {
    ProviderDecl {
        id: PROVIDER_ID.to_string(),
        name: "Model router".to_string(),
        transport: ProviderTransportDecl {
            kind: "openai-responses".to_string(),
            base_url: "https://127.0.0.1:1/"
                .parse()
                .expect("loopback placeholder"),
            authorization: gray_plugin::ProviderAuthorizationDecl {
                kind: "bearer".to_string(),
                secret_name: "relay_token".to_string(),
            },
            request: gray_plugin::ProviderRequestPolicyDecl {
                prompt_cache_key: false,
                warm_replay: false,
                store: false,
                include_reasoning_encrypted: true,
                previous_response_id: false,
                tool_choice: Some("auto".to_string()),
                parallel_tool_calls: Some(true),
                text_verbosity: None,
                cache_ttl_secs: None,
            },
            headers: Vec::new(),
        },
        auth_methods: vec![AuthMethodDecl {
            id: AUTH_METHOD_ID.to_string(),
            name: "router.json tiers".to_string(),
            kind: "api_key".to_string(),
            operations: vec!["models".to_string(), "chat".to_string()],
        }],
    }
}

const USAGE: &str = "/route — tiered model routing.\n\
     /route fast|smart|cheap  pin every turn to a tier\n\
     /route auto              classify each request (default)\n\
     /route off               no routing: forward to the default tier\n\
     /route status            show mode, tiers, last decision\n\
     /route classify <text>   dry-run the classifier on a prompt\n\
     /route setup             write a template ~/.gray/router.json\n\
     /route use               open the model picker on router rows";

/// `command/run` for `/route`. `argv` excludes the command name.
/// `None` = the host falls through to its provider-login shortcut.
pub fn run_command(argv: &[String]) -> Option<Value> {
    let cfg = RouterConfig::load().ok();
    let state = RouterState::load();
    let verb = argv.first().map(|s| s.as_str()).unwrap_or("status");
    match verb {
        "status" | "help" => Some(json!({"text": status(cfg.as_ref(), &state)})),
        "auto" | "off" => set_mode(verb, cfg.as_ref()),
        "use" | "connect" => Some(json!({
            "model_picker": AUTO_ROW,
            "text": "Open /model and pick a Router row (this gray is too old for /route use)",
        })),
        "setup" => Some(json!({"text": crate::setup::setup(false)})),
        "classify" => {
            let text = argv.get(1..).unwrap_or(&[]).join(" ");
            Some(json!({"text": classify_text(&text, cfg.as_ref())}))
        }
        name if config::TIER_NAMES.contains(&name)
            || cfg.as_ref().map(|c| c.tiers.contains_key(name)) == Some(true) =>
        {
            set_mode(name, cfg.as_ref())
        }
        _ => Some(json!({"text": USAGE})),
    }
}

fn set_mode(mode: &str, cfg: Option<&RouterConfig>) -> Option<Value> {
    if mode != "auto" && mode != "off" {
        match cfg {
            Some(c) if c.tiers.contains_key(mode) => {}
            _ => {
                return Some(json!({"text": format!(
                    "tier \"{mode}\" is not configured in ~/.gray/router.json"
                )}));
            }
        }
    }
    match config::set_mode(mode) {
        Ok(()) => Some(json!({"text": format!("router mode: {mode}")})),
        Err(e) => Some(json!({"text": format!("router: cannot save state: {e}")})),
    }
}

fn status(cfg: Option<&RouterConfig>, state: &RouterState) -> String {
    let mut out = String::new();
    match cfg {
        None => {
            out.push_str("router: not configured — `/route setup` writes ~/.gray/router.json\n")
        }
        Some(c) => {
            out.push_str(&format!("router mode: {}\n", state.mode));
            for name in c.tier_names() {
                let t = &c.tiers[&name];
                let key = if t.api_key().is_some() {
                    "key ✓"
                } else {
                    "no key"
                };
                out.push_str(&format!(
                    "  {name:6} {}  ({}, {})\n",
                    t.model,
                    t.responses_url(),
                    key
                ));
            }
            out.push_str(&format!(
                "  default_tier: {}\n",
                c.fallback_tier().unwrap_or_else(|| "(none)".into())
            ));
            if let Some(last) = &state.last {
                out.push_str(&format!(
                    "  last: {} → {} ({})\n",
                    last.tier, last.model, last.reason
                ));
            }
            if c.announce {
                out.push_str("  announce: on\n");
            }
        }
    }
    out.push_str(USAGE);
    out
}

/// `/route classify …`: run the heuristic on raw text — no request needed.
fn classify_text(text: &str, cfg: Option<&RouterConfig>) -> String {
    let Some(cfg) = cfg else {
        return "router: not configured — `/route setup` first".to_string();
    };
    // Feed the text through the same body shape a real turn carries.
    let body =
        json!({"input": [{"role": "user", "content": [{"type": "input_text", "text": text}]}]});
    let s = crate::classify::signals(&body, &cfg.rules);
    let (score, why) = crate::classify::score(&s, &cfg.rules);
    let tier = crate::classify::pick(score, cfg).unwrap_or_else(|| "(none)".into());
    let model = cfg
        .tiers
        .get(&tier)
        .map(|t| t.model.clone())
        .unwrap_or_default();
    format!("score {score} → {tier} ({model})\n{}", why.join("\n"))
}

/// `prompt/context` line: one quiet status so the model knows routing is
/// on and where `/route` lives. Empty when unconfigured.
pub fn prompt_context() -> Value {
    let Ok(_cfg) = RouterConfig::load() else {
        return json!({"text": ""});
    };
    let state = RouterState::load();
    json!({"text": format!(
        "model router active: mode={} — /route <fast|smart|cheap|auto|off|status> changes it.",
        state.mode
    )})
}
