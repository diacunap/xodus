//! Account-scoped lists (owned games, Game Pass), backed by `xodus::api::collections`.

use xodus::api::collections;
use xodus::tokens::TokenManager;

use crate::catalog::Error;

/// Owned product ids with how they were acquired ("purchased" | "gamepass" | "other").
pub async fn owned(
    client: &reqwest::Client,
    tokens: &TokenManager,
) -> Result<Vec<(String, String)>, Error> {
    let products = collections::get_owned_products(client, tokens).await?;
    Ok(products
        .into_iter()
        .map(|p| {
            let acquisition = serde_json::to_value(p.acquisition)?
                .as_str()
                .unwrap_or("other")
                .to_owned();
            Ok((p.product_id, acquisition))
        })
        .collect::<Result<_, serde_json::Error>>()?)
}

pub async fn gamepass(client: &reqwest::Client) -> Result<Vec<String>, Error> {
    collections::get_gamepass_pc_product_ids(client, "US").await
}
