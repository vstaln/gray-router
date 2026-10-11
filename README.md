# gray-router

Tiered model routing for [gray](https://github.com/vstaln/gray) — a
protocol-1.2 **provider plugin** that sends each request of a session to a
different upstream model based on configured tiers and a cheap
per-request classifier. Port of the pi `pi-bifrost` / `auto-model-router`
idea, reimplemented on gray's provider sidecar surface.

## How it works (the model-switch mechanism)

Gray's wire has no "set session model" RPC: `/model` is user-only
(it persists to `~/.gray/config.json` and rebuilds the agent), and a
plugin `command/run` reply can only *open* the picker (`model_picker`),
run a prompt (`prompt`), or print (`text`). The one surface that truly
controls which model serves a request is the **provider path** itself:

- The manifest declares the `router` provider (`openai-responses`
  transport, placeholder base URL) with model rows `auto`, `fast`,
  `smart`, `cheap` (one row per configured tier, plus `auto`).
- Per turn, the host calls `provider/chat`; the sidecar opens a one-shot
  loopback relay and returns `relay_url`/`relay_token`. The host POSTs
  its standard Responses body there.
- The admitted POST is **routed**: `/route` mode pins a tier or turns
  routing off; `auto` classifies the request (heuristic, or an optional
  LLM call behind a flag); an explicitly picked tier row always wins.
  The relay rewrites `model` to the tier's upstream model, drops
  `previous_response_id` (reply chains can't cross models), forwards the
  body verbatim to `tier.base_url + /responses` with the tier's own
  bearer, and streams upstream SSE back untouched — usage, reasoning
  items and tool calls survive intact.
- Single admission per relay, per-turn bearer, `: keepalive` SSE comments
  while slow upstreams think. Same shape as `gray-devin-sub`'s relay.

So the "session model" is a router row; the real model is chosen per
request, mid-session, without rebuilding the agent.

## Config — `~/.gray/router.json`

```json
{
  "tiers": {
    "fast":  {"base_url": "https://api.openai.com/v1", "model": "gpt-5-mini",
              "api_key_env": "OPENAI_API_KEY"},
    "smart": {"base_url": "https://api.openai.com/v1", "model": "gpt-5.2",
              "api_key_env": "OPENAI_API_KEY"},
    "cheap": {"base_url": "http://127.0.0.1:4000/v1", "model": "local-small",
              "api_key": "sk-local"}
  },
  "default_tier": "smart",
  "classifier": {"kind": "heuristic", "tier": "fast"},
  "announce": true,
  "rules": {
    "fast_max_chars": 200, "smart_min_chars": 1500, "smart_min_tools": 4,
    "trivial_score_max": 30, "complex_score_min": 66,
    "smart_keywords": ["refactor", "..."],
    "trivial_keywords": ["hi", "thanks", "..."]
  }
}
```

- Every tier needs `base_url` (the relay appends `/responses`), `model`,
  and a key: `api_key_env` (env var name — preferred, nothing secret on
  disk) or `api_key` (literal; fine for local gateways). `headers` adds
  upstream headers (e.g. OpenRouter attribution).
- Tier names beyond `cheap`/`fast`/`smart` are allowed; the three bands
  map cheapest→smartest across whatever is configured.
- Upstream must speak the **OpenAI Responses API** — the body is
  forwarded as-is, so chat-completions-only endpoints won't work
  (a local gateway like LiteLLM/OmniRoute or api.openai.com does).

**Mode/state** lives in `~/.gray/router.state.json` (`mode`, last
decision) — written atomically by `/route` and the relay; read fresh
every turn, so `/route` takes effect on the next request.

## `/route`

| command | effect |
|---|---|
| `/route fast\|smart\|cheap` | pin every turn to a tier |
| `/route auto` | classify each request (default) |
| `/route off` | no routing — forward to `default_tier` (or the picked tier row) |
| `/route status` | mode, tiers, keys present, last decision |
| `/route classify <text>` | dry-run the classifier on a prompt |
| `/route setup` | write a template `router.json` |
| `/route use` | open the model picker on the router rows |

`prompt/context` adds one quiet status line per turn while configured.

## Classifier

Zero extra calls by default. Signals from the request body — last user
message length, tool count, history depth, code fences, technical
keywords, whole-message acknowledgements — score 0–100 into three bands:
≤ `trivial_score_max` → cheapest tier, ≥ `complex_score_min` → strongest,
middle → `fast`-ish. `classifier.kind = "llm"` swaps in one tiny
non-streaming call on `classifier.tier` per turn; failure falls back to
the heuristic.

## Install

```console
$ cargo build --release
$ gray plugin install ~/grayplugins/gray-router/target/release/gray-router
$ gray-router setup            # writes ~/.gray/router.json — edit tiers in
```

Then in a session: `/connect` → *Model router* (external login; connect
just checks `router.json` exists) → pick **`auto`**. Or `/route use`.

## Limits

- Upstream must be an OpenAI **Responses** endpoint; no Anthropic
  translation (kind is `openai-responses` only on this wire).
- `previous_response_id` is always stripped; encrypted reasoning replays
  only to the provider that minted it, so cross-model continuations may
  lose reasoning blobs.
- Relay admits one request and speaks `Content-Length` POSTs — that is
  the host's wire; it is not a general proxy.
- `/route` changes take effect next turn; an in-flight turn keeps its
  relay decision.
- Full e2e against real tiers needs the tier endpoints + keys — see
  "needs credentials" below; `tests/e2e.rs` covers the whole wire path
  with mock upstreams (manifest → provider/chat → relay POST → routed
  upstream) without any.

## Dev

`cargo build`, `cargo test`, `gray plugin check target/debug/gray-router`.
Files: `src/relay.rs` (loopback relay + upstream proxy + classifier
call), `src/classify.rs` (heuristic), `src/config.rs` (`router.json` /
state), `src/catalog.rs` (model rows), `src/manifest.rs` (provider decl +
`/route`), `src/main.rs` (stdio wire loop), `src/setup.rs` (template).
