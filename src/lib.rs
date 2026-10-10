//! `gray_router`: the sidecar core for the tiered model router plugin.
//!
//! Router is a protocol-1.2 provider plugin: it declares the `router`
//! provider whose model rows are the tiers (`auto`, `fast`, `smart`,
//! `cheap`). Each `provider/chat` call parks the selected row and opens a
//! per-turn loopback relay; the admitted Responses POST is classified,
//! rewritten to the winning tier's upstream model, and forwarded to that
//! tier's `base_url` with its own credential. That relay is the only wire
//! surface that can send one request of a session to a different model:
//! `/model` is user-driven and `command/run` outcomes can only open the
//! picker, so routing lives in the provider path (see README).

pub mod catalog;
pub mod classify;
pub mod config;
pub mod manifest;
pub mod relay;
pub mod setup;
