//! `~/.gray/router.json` — the user-owned routing table.
//!
//! ```json
//! {
//!   "tiers": {
//!     "fast":  {"base_url": "https://api.openai.com/v1", "model": "gpt-5-mini",
//!               "api_key_env": "OPENAI_API_KEY"},
//!     "smart": {"base_url": "https://api.openai.com/v1", "model": "gpt-5.2",
//!               "api_key_env": "OPENAI_API_KEY"},
//!     "cheap": {"base_url": "http://127.0.0.1:4000/v1", "model": "local-small",
//!               "api_key": "sk-local"}
//!   },
//!   "mode": "auto",
//!   "default_tier": "smart",
//!   "classifier": {"kind": "heuristic", "tier": "fast"},
//!   "announce": false,
//!   "rules": { "smart_min_chars": 1500, "smart_keywords": [...] }
//! }
//! ```
//!
//! `mode` lives in the same file so `/route` edits one place; it is the
//! only field the plugin writes (atomically). Keys resolve per turn:
//! `api_key_env` names an env var (preferred — nothing secret lands on
//! disk), `api_key` is a literal for local/dev endpoints.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;

/// Canonical tier names, cheapest to strongest.
pub const TIER_NAMES: [&str; 3] = ["cheap", "fast", "smart"];
/// The row that asks for per-request classification.
pub const AUTO_ROW: &str = "auto";
/// Config file name under the gray home.
pub const CONFIG_FILE: &str = "router.json";
/// Mode/state file name (`/route` writes mode here, not into the config).
pub const STATE_FILE: &str = "router.state.json";

pub fn home() -> Result<PathBuf> {
    std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".gray")))
        .context("cannot resolve Gray home")
}

pub fn config_path() -> Result<PathBuf> {
    Ok(home()?.join(CONFIG_FILE))
}

pub fn state_path() -> Result<PathBuf> {
    Ok(home()?.join(STATE_FILE))
}

/// One upstream endpoint a tier name maps to.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Tier {
    /// OpenAI Responses base URL; the relay appends `/responses`.
    pub base_url: String,
    /// Upstream model id sent in the forwarded request body.
    pub model: String,
    /// Name of the environment variable holding the upstream key.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Literal key (discouraged for real providers; handy for local
    /// gateways that still want a bearer, or a throwaway dev key).
    #[serde(default)]
    pub api_key: Option<String>,
    /// Extra headers forwarded upstream (e.g. OpenRouter attribution).
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

impl Tier {
    /// The URL the relay POSTs the rewritten body to.
    pub fn responses_url(&self) -> String {
        format!("{}/responses", self.base_url.trim_end_matches('/'))
    }

    /// Resolve the bearer for this tier: env first, then the literal.
    pub fn api_key(&self) -> Option<String> {
        if let Some(var) = &self.api_key_env {
            if let Ok(v) = std::env::var(var) {
                if !v.trim().is_empty() {
                    return Some(v);
                }
            }
        }
        self.api_key.clone().filter(|k| !k.trim().is_empty())
    }
}

/// Heuristic knobs; every field optional, documented defaults in
/// [`Rules::default`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Rules {
    /// Prompt shorter than this (chars) leans down one level.
    #[serde(default = "d_fast_max_chars")]
    pub fast_max_chars: usize,
    /// Prompt longer than this (chars) leans smart.
    #[serde(default = "d_smart_min_chars")]
    pub smart_min_chars: usize,
    /// Advertised tool count at or above this leans smart.
    #[serde(default = "d_smart_min_tools")]
    pub smart_min_tools: usize,
    /// Score at or below this routes to the cheapest configured tier.
    #[serde(default = "d_trivial_max")]
    pub trivial_score_max: u32,
    /// Score at or above this routes to the strongest configured tier.
    #[serde(default = "d_complex_min")]
    pub complex_score_min: u32,
    /// Whole-word hits that push the score up (long-tail technical words).
    #[serde(default = "d_smart_keywords")]
    pub smart_keywords: Vec<String>,
    /// Whole-message matches that floor the score (acknowledgements).
    #[serde(default = "d_trivial_keywords")]
    pub trivial_keywords: Vec<String>,
}

fn d_fast_max_chars() -> usize {
    200
}
fn d_smart_min_chars() -> usize {
    1500
}
fn d_smart_min_tools() -> usize {
    4
}
fn d_trivial_max() -> u32 {
    30
}
fn d_complex_min() -> u32 {
    66
}
fn d_smart_keywords() -> Vec<String> {
    [
        "refactor", "architect", "architecture", "optimize", "optimization",
        "debug", "migrate", "migration", "implement", "design", "review",
        "analyze", "analyse", "benchmark", "security", "concurrency",
        "distributed", "compile", "compiler", "algorithm", "regression",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}
fn d_trivial_keywords() -> Vec<String> {
    [
        "hi", "hello", "hey", "thanks", "thank you", "ok", "okay", "yes",
        "no", "yep", "nope", "sure", "lgtm", "cool", "nice",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

impl Default for Rules {
    fn default() -> Self {
        Self {
            fast_max_chars: d_fast_max_chars(),
            smart_min_chars: d_smart_min_chars(),
            smart_min_tools: d_smart_min_tools(),
            trivial_score_max: d_trivial_max(),
            complex_score_min: d_complex_min(),
            smart_keywords: d_smart_keywords(),
            trivial_keywords: d_trivial_keywords(),
        }
    }
}

/// How `auto` picks a tier.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Classifier {
    /// "heuristic" (default, zero extra calls) or "llm" (one tiny upstream
    /// call per turn on `tier`).
    #[serde(default = "d_classifier_kind")]
    pub kind: String,
    /// Tier whose upstream answers the classification prompt (`llm` kind).
    #[serde(default = "d_classifier_tier")]
    pub tier: String,
}

fn d_classifier_kind() -> String {
    "heuristic".to_string()
}
fn d_classifier_tier() -> String {
    "fast".to_string()
}

impl Default for Classifier {
    fn default() -> Self {
        Self {
            kind: d_classifier_kind(),
            tier: d_classifier_tier(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RouterConfig {
    #[serde(default)]
    pub tiers: BTreeMap<String, Tier>,
    /// Tier the router falls back to when routing is off or the picked
    /// tier isn't configured. Defaults to the strongest configured tier.
    #[serde(default)]
    pub default_tier: Option<String>,
    #[serde(default)]
    pub classifier: Classifier,
    /// Emit a `host/say` line when auto picks a different tier than last
    /// time (visibility into where turns go). Off by default.
    #[serde(default)]
    pub announce: bool,
    #[serde(default)]
    pub rules: Rules,
}

impl RouterConfig {
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                anyhow::bail!(
                    "router is not configured — run `gray-router setup` to write {}",
                    path.display()
                )
            }
            Err(e) => return Err(e.into()),
        };
        let cfg: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid {}", path.display()))?;
        Ok(cfg)
    }

    /// Configured tier names in canonical order (cheap, fast, smart first,
    /// any custom names after).
    pub fn tier_names(&self) -> Vec<String> {
        let mut names: Vec<String> = TIER_NAMES
            .iter()
            .filter(|t| self.tiers.contains_key(**t))
            .map(|t| t.to_string())
            .collect();
        for k in self.tiers.keys() {
            if !names.iter().any(|n| n == k) {
                names.push(k.clone());
            }
        }
        names
    }

    /// The tier to use when a pick lands on a name that isn't configured
    /// (or routing is off): `default_tier` if configured, else strongest.
    pub fn fallback_tier(&self) -> Option<String> {
        if let Some(d) = &self.default_tier {
            if self.tiers.contains_key(d) {
                return Some(d.clone());
            }
        }
        self.tier_names().last().cloned()
    }
}

/// `/route`-owned state: mode + the last decision for `/route status`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RouterState {
    /// "auto" | "off" | a tier name. Missing = "auto".
    #[serde(default = "d_mode")]
    pub mode: String,
    /// Last routing decision (display only).
    #[serde(default)]
    pub last: Option<LastDecision>,
}

fn d_mode() -> String {
    "auto".to_string()
}

impl Default for RouterState {
    fn default() -> Self {
        Self {
            mode: d_mode(),
            last: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LastDecision {
    pub tier: String,
    pub model: String,
    /// Heuristic score or "llm" / "pinned" / "off" — why this tier.
    pub reason: String,
    pub at: String,
}

impl RouterState {
    pub fn load() -> Self {
        let Ok(bytes) = std::fs::read(state_path().unwrap_or_default()) else {
            return Self::default();
        };
        serde_json::from_slice(&bytes).unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        let path = state_path()?;
        let dir = path.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&dir)?;
        let mut tmp = tempfile::NamedTempFile::new_in(&dir)?;
        writeln!(tmp, "{}", serde_json::to_string_pretty(self)?)?;
        tmp.persist(&path)?;
        Ok(())
    }

    /// Record a routing decision (best-effort: a status write must never
    /// fail a turn).
    pub fn record(&mut self, tier: &str, model: &str, reason: &str) {
        self.last = Some(LastDecision {
            tier: tier.to_string(),
            model: model.to_string(),
            reason: reason.to_string(),
            at: now_stamp(),
        });
        let _ = self.save();
    }
}

fn now_stamp() -> String {
    // RFC-3339-ish without chrono: unix seconds + the host only displays it.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("unix:{secs}")
}

/// `/route <mode>` — persist a new mode. Validated by the caller.
pub fn set_mode(mode: &str) -> Result<()> {
    let mut state = RouterState::load();
    state.mode = mode.to_string();
    state.save()
}

/// Template written by `gray-router setup`: real shape, placeholder
/// endpoints, no secrets.
pub fn template() -> Value {
    json!({
        "tiers": {
            "fast": {
                "base_url": "https://api.openai.com/v1",
                "model": "gpt-5-mini",
                "api_key_env": "OPENAI_API_KEY"
            },
            "smart": {
                "base_url": "https://api.openai.com/v1",
                "model": "gpt-5.2",
                "api_key_env": "OPENAI_API_KEY"
            }
        },
        "default_tier": "smart",
        "classifier": {"kind": "heuristic", "tier": "fast"},
        "announce": false,
        "rules": {
            "fast_max_chars": 200,
            "smart_min_chars": 1500,
            "smart_min_tools": 4,
            "trivial_score_max": 30,
            "complex_score_min": 66
        }
    })
}
