//! `provider/models`: the router's catalog. `auto` is always listed;
//! every configured tier is its own row (selecting one pins the session
//! to that upstream without going through the classifier).

use gray_plugin::{ProviderModel, ProviderModelCatalog};

use crate::config::{RouterConfig, AUTO_ROW};

pub fn catalog(cfg: Option<&RouterConfig>) -> ProviderModelCatalog {
    let mut models = vec![ProviderModel {
        id: AUTO_ROW.to_string(),
        name: "Auto (classify per request)".to_string(),
        context_window: None,
        reasoning_efforts: Vec::new(),
        variants: Vec::new(),
        slots: Vec::new(),
        price_cliff_tokens: None,
    }];
    if let Some(cfg) = cfg {
        for name in cfg.tier_names() {
            let tier = &cfg.tiers[&name];
            models.push(ProviderModel {
                id: name.clone(),
                name: format!("{name} → {}", tier.model),
                context_window: None,
                reasoning_efforts: Vec::new(),
                variants: Vec::new(),
                slots: Vec::new(),
                price_cliff_tokens: None,
            });
        }
    }
    ProviderModelCatalog { models }
}
