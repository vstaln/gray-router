//! The `auto` classifier: a cheap heuristic, never an LLM call unless
//! `classifier.kind = "llm"` opts in (the relay owns that path).
//!
//! Signals come straight off the Responses request body: the last user
//! message's text, the advertised tool count, and history depth. The score
//! is 0–100; `rules.trivial_score_max` / `complex_score_min` split it into
//! three bands that map onto the configured tiers.

use serde_json::Value;

use crate::config::{RouterConfig, Rules};

/// What the heuristic scored from one request body.
#[derive(Clone, Debug, Default)]
pub struct Signals {
    /// Chars in the last user message's text.
    pub prompt_chars: usize,
    /// `input` items — a long-running session already carries weight.
    pub history_items: usize,
    /// Tools advertised for this request.
    pub tool_count: usize,
    /// The message is one short line ending in `?`.
    pub short_question: bool,
    /// A fenced code block or stack-trace-ish line in the prompt.
    pub codeish: bool,
    /// Whole-word hits on `rules.smart_keywords`.
    pub smart_hits: usize,
    /// The whole message is a trivial acknowledgement word.
    pub trivial_message: bool,
}

/// Last `role: "user"` item's concatenated text (`input_text` parts or a
/// bare string content). Tool outputs and assistant items are skipped —
/// they must not push a trivial prompt into "complex" just for being long.
pub fn last_user_text(body: &Value) -> String {
    let Some(items) = body.get("input").and_then(|i| i.as_array()) else {
        return String::new();
    };
    for item in items.iter().rev() {
        let role = item.get("role").and_then(|r| r.as_str()).unwrap_or("");
        if role != "user" {
            continue;
        }
        let mut out = String::new();
        match item.get("content") {
            Some(Value::String(s)) => out = s.clone(),
            Some(Value::Array(parts)) => {
                for p in parts {
                    if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(t);
                    }
                }
            }
            _ => {}
        }
        return out;
    }
    String::new()
}

pub fn signals(body: &Value, rules: &Rules) -> Signals {
    let text = last_user_text(body);
    let trimmed = text.trim();
    let history_items = body
        .get("input")
        .and_then(|i| i.as_array())
        .map(|i| i.len())
        .unwrap_or(0);
    let tool_count = body
        .get("tools")
        .and_then(|t| t.as_array())
        .map(|t| t.len())
        .unwrap_or(0);
    let lower = trimmed.to_lowercase();
    let trivial_message =
        !lower.is_empty() && rules.trivial_keywords.iter().any(|w| lower == w.as_str());
    let smart_hits = if trivial_message {
        0
    } else {
        rules
            .smart_keywords
            .iter()
            .filter(|k| contains_word(&lower, k))
            .count()
    };
    Signals {
        prompt_chars: trimmed.chars().count(),
        history_items,
        tool_count,
        short_question: trimmed.ends_with('?') && !trimmed.contains('\n'),
        codeish: trimmed.contains("```") || trimmed.contains("Traceback"),
        smart_hits,
        trivial_message,
    }
}

/// Word-boundary `contains` (keywords are lowercase words/phrases).
fn contains_word(haystack: &str, needle: &str) -> bool {
    for (i, _) in haystack.match_indices(needle) {
        let before_ok = i == 0
            || !haystack[..i]
                .chars()
                .next_back()
                .map(|c| c.is_alphanumeric() || c == '_')
                .unwrap_or(false);
        let after = &haystack[i + needle.len()..];
        let after_ok = after
            .chars()
            .next()
            .map(|c| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(true);
        if before_ok && after_ok {
            return true;
        }
    }
    false
}

/// Score 0–100. ~45 is neutral; each signal pushes toward a band.
pub fn score(s: &Signals, rules: &Rules) -> (u32, Vec<String>) {
    let mut v: i64 = 45;
    let mut why = Vec::new();
    let mut bump = |v: &mut i64, d: i64, reason: String| {
        *v += d;
        why.push(reason);
    };
    if s.trivial_message {
        bump(&mut v, -35, "trivial message".into());
    }
    // Length discounts apply only when nothing technical was named:
    // a dense request ("refactor the compiler") is small but not trivial.
    let technical = s.smart_hits > 0;
    if s.prompt_chars < 80 && !technical {
        bump(
            &mut v,
            -20,
            format!("short prompt ({} chars)", s.prompt_chars),
        );
    } else if s.prompt_chars < rules.fast_max_chars && !technical {
        bump(
            &mut v,
            -12,
            format!("small prompt ({} chars)", s.prompt_chars),
        );
    } else if s.prompt_chars > rules.smart_min_chars {
        bump(
            &mut v,
            30,
            format!("long prompt ({} chars)", s.prompt_chars),
        );
    } else if s.prompt_chars > rules.fast_max_chars * 2 {
        bump(
            &mut v,
            10,
            format!("medium prompt ({} chars)", s.prompt_chars),
        );
    }
    if s.short_question && s.prompt_chars < 80 {
        bump(&mut v, -8, "short question".into());
    }
    if s.codeish {
        bump(&mut v, 12, "code/trace in prompt".into());
    }
    if s.tool_count >= rules.smart_min_tools {
        bump(&mut v, 16, format!("{} tools advertised", s.tool_count));
    } else if s.tool_count > 0 {
        bump(&mut v, 8, format!("{} tools advertised", s.tool_count));
    }
    if s.history_items > 8 {
        bump(
            &mut v,
            8,
            format!("deep history ({} items)", s.history_items),
        );
    }
    for _ in 0..s.smart_hits.min(3) {
        bump(&mut v, 12, "technical keyword".into());
    }
    let clamped = v.clamp(0, 100) as u32;
    (clamped, why)
}

/// Score → tier band → configured tier name (with fallback).
pub fn pick(score: u32, cfg: &RouterConfig) -> Option<String> {
    let rules = &cfg.rules;
    let pref: &[&str] = if score <= rules.trivial_score_max {
        // Trivial: cheapest configured tier first.
        &["cheap", "fast", "smart"]
    } else if score >= rules.complex_score_min {
        &["smart", "fast", "cheap"]
    } else {
        &["fast", "cheap", "smart"]
    };
    for name in pref {
        if cfg.tiers.contains_key(*name) {
            return Some((*name).to_string());
        }
    }
    cfg.fallback_tier()
}

/// Full `auto` decision for a request body.
pub fn classify(body: &Value, cfg: &RouterConfig) -> Option<(String, String)> {
    let s = signals(body, &cfg.rules);
    let (score, why) = score(&s, &cfg.rules);
    let tier = pick(score, cfg)?;
    Some((tier, format!("auto score {score} ({})", why.join(", "))))
}
