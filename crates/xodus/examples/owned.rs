//! Manual check against the live, signed-in account:
//! `cargo run --release -p xodus --example owned`
use xodus::api::collections::{Acquisition, get_gamepass_pc_product_ids, get_owned_products};
use xodus::tokens::TokenManager;

const NO_MANS_SKY: &str = "BQVQTL3PCH05";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    xodus::secrets::init_secrets()?;
    let tokens = TokenManager::with_default_storage();
    let client = reqwest::Client::new();
    xodus::tokens::device::ensure_device_credentials(&client, &tokens).await;

    let owned = get_owned_products(&client, &tokens).await?;
    let count = |a: Acquisition| owned.iter().filter(|p| p.acquisition == a).count();
    println!(
        "owned games: {} (purchased={} gamepass={} other={})",
        owned.len(),
        count(Acquisition::Purchased),
        count(Acquisition::GamePass),
        count(Acquisition::Other)
    );
    for p in owned.iter().take(15) {
        println!("  {} {:?}", p.product_id, p.acquisition);
    }

    let gamepass = get_gamepass_pc_product_ids(&client, "US").await?;
    println!("game pass pc games: {}", gamepass.len());

    let nms_owned = owned.iter().find(|p| p.product_id == NO_MANS_SKY);
    println!(
        "{NO_MANS_SKY}: owned={:?} in_gamepass_pc={}",
        nms_owned.map(|p| p.acquisition),
        gamepass.iter().any(|id| id == NO_MANS_SKY)
    );
    Ok(())
}
