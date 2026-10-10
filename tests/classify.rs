//! Classifier units: the required "trivial → fast, complex → smart"
//! property plus signal extraction.

use gray_router::classify;
use gray_router::config::{RouterConfig, Rules, Tier};
use serde_json::json;
use std::collections::BTreeMap;

fn tier(model: &str) -> Tier {
    Tier {
        base_url: "http://127.0.0.1:1".into(),
        model: model.into(),
        api_key_env: None,
        api_key: Some("k".into()),
        headers: BTreeMap::new(),
    }
}

fn cfg(tiers: &[&str]) -> RouterConfig {
    RouterConfig {
        tiers: tiers
            .iter()
            .map(|t| (t.to_string(), tier(t)))
            .collect::<BTreeMap<_, _>>(),
        ..RouterConfig::default()
    }
}

fn body(text: &str) -> serde_json::Value {
    json!({"input": [{"role": "user", "content": [{"type": "input_text", "text": text}]}]})
}

fn decide(text: &str, cfg: &RouterConfig) -> String {
    classify::classify(&body(text), cfg).unwrap().0
}

#[test]
fn trivial_prompt_picks_fast() {
    let c = cfg(&["fast", "smart"]);
    assert_eq!(decide("hi", &c), "fast");
    assert_eq!(decide("what is a trait?", &c), "fast");
}

#[test]
fn complex_prompt_picks_smart() {
    let c = cfg(&["fast", "smart"]);
    let long = format!(
        "Refactor this codebase's concurrency architecture: {} \
         ```rust\nfn main() {{}}\n``` analyze the migration and implement the design",
        "x".repeat(2000)
    );
    assert_eq!(decide(&long, &c), "smart");
}

#[test]
fn trivial_prefers_cheap_when_configured() {
    let c = cfg(&["cheap", "fast", "smart"]);
    assert_eq!(decide("thanks", &c), "cheap");
}

#[test]
fn smart_keywords_lift_score() {
    let c = cfg(&["fast", "smart"]);
    let r = Rules::default();
    let s = classify::signals(
        &body("please refactor the compiler architecture and optimize the algorithm"),
        &r,
    );
    let (score, _) = classify::score(&s, &r);
    assert!(score >= r.complex_score_min, "score {score}");
}

#[test]
fn last_user_text_skips_tool_outputs() {
    let b = json!({"input": [
        {"role": "user", "content": [{"type": "input_text", "text": "first"}]},
        {"role": "assistant", "content": [{"type": "output_text", "text": "mid"}]},
        {"type": "function_call_output", "output": "tool output text"},
        {"role": "user", "content": [{"type": "input_text", "text": "latest"}]}
    ]});
    assert_eq!(classify::last_user_text(&b), "latest");
}

#[test]
fn word_boundaries() {
    let r = Rules::default();
    let s = classify::signals(&body("the implementation detail"), &r);
    // "implement" must not match inside "implementation".
    assert_eq!(s.smart_hits, 0);
}
