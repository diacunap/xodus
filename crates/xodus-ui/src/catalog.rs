//! Public Microsoft Store catalog lookups (no auth) shaped for the UI.

use futures_util::future::join_all;
use serde::Serialize;
use serde_json::Value;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

const MARKET: &str = "US";
const LANG: &str = "en-US";
/// displaycatalog accepts a limited number of bigIds per request.
const BATCH: usize = 20;

#[derive(Debug, Clone, Serialize)]
pub struct Game {
    pub product_id: String,
    pub title: String,
    pub publisher: Option<String>,
    pub image: Option<String>,
    pub kind: Option<String>,
    /// Best Windows.Desktop package, if the product ships one.
    pub pc: Option<PcPackage>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PcPackage {
    pub format: String,
    pub size_bytes: u64,
    pub content_id: Option<String>,
    /// "supported" | "unsupported" | "not_gdk"
    pub xodus: &'static str,
}

/// Store search (same endpoint the Store app autosuggest uses).
pub async fn search(client: &reqwest::Client, query: &str) -> Result<Vec<Game>, Error> {
    let resp: Value = client
        .get("https://displaycatalog.mp.microsoft.com/v7.0/productFamilies/autosuggest")
        .query(&[
            ("market", MARKET),
            ("languages", LANG),
            ("query", query),
            ("productFamilyNames", "Games"),
        ])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let ids: Vec<String> = resp["Results"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|r| r["Products"].as_array().into_iter().flatten())
        .filter(|p| p["Type"].as_str() == Some("Game"))
        .filter_map(|p| p["ProductId"].as_str().map(str::to_owned))
        .collect();

    products(client, &ids).await
}

/// Resolve product ids to UI cards, preserving input order.
pub async fn products(client: &reqwest::Client, ids: &[String]) -> Result<Vec<Game>, Error> {
    let batches = ids.chunks(BATCH).map(|chunk| fetch_batch(client, chunk));
    let mut by_id = std::collections::HashMap::new();
    for batch in join_all(batches).await {
        for game in batch? {
            by_id.insert(game.product_id.clone(), game);
        }
    }
    Ok(ids.iter().filter_map(|id| by_id.remove(id)).collect())
}

async fn fetch_batch(client: &reqwest::Client, ids: &[String]) -> Result<Vec<Game>, Error> {
    let resp: Value = client
        .get("https://displaycatalog.mp.microsoft.com/v7.0/products")
        .query(&[
            ("bigIds", ids.join(",").as_str()),
            ("market", MARKET),
            ("languages", LANG),
        ])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    Ok(resp["Products"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(parse_product)
        .collect())
}

fn parse_product(p: &Value) -> Option<Game> {
    let loc = &p["LocalizedProperties"][0];
    Some(Game {
        product_id: p["ProductId"].as_str()?.to_owned(),
        title: loc["ProductTitle"].as_str().unwrap_or("?").to_owned(),
        publisher: loc["PublisherName"].as_str().map(str::to_owned),
        image: pick_image(&loc["Images"]),
        kind: p["ProductKind"].as_str().map(str::to_owned),
        pc: pc_package(p),
    })
}

fn pick_image(images: &Value) -> Option<String> {
    let images = images.as_array()?;
    ["Poster", "BoxArt", "Tile", "Logo"]
        .iter()
        .find_map(|purpose| {
            images
                .iter()
                .find(|i| i["ImagePurpose"].as_str() == Some(*purpose))
        })
        .and_then(|i| i["Uri"].as_str())
        .map(|uri| {
            let uri = if uri.starts_with("//") { format!("https:{uri}") } else { uri.to_owned() };
            // Ask the image CDN for a thumbnail instead of the full-size asset.
            format!("{uri}?w=300")
        })
}

fn pc_package(p: &Value) -> Option<PcPackage> {
    p["DisplaySkuAvailabilities"]
        .as_array()?
        .iter()
        .flat_map(|dsa| dsa["Sku"]["Properties"]["Packages"].as_array().into_iter().flatten())
        .filter(|pkg| {
            pkg["PlatformDependencies"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|d| d["PlatformName"].as_str() == Some("Windows.Desktop"))
        })
        .map(|pkg| {
            let format = pkg["PackageFormat"].as_str().unwrap_or("?").to_owned();
            let xodus = match format.as_str() {
                "MSIXVC" => "supported",
                "MSIXVC2" => "unsupported",
                _ => "not_gdk",
            };
            PcPackage {
                size_bytes: pkg["MaxDownloadSizeInBytes"].as_u64().unwrap_or(0),
                content_id: pkg["ContentId"].as_str().map(str::to_owned),
                format,
                xodus,
            }
        })
        .max_by_key(|pkg| pkg.size_bytes)
}
