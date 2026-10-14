//! Games associated with the signed-in account.
//!
//! * Owned entitlements come from the Store collections service
//!   (`collections.mp.microsoft.com/v7.0/collections/query`). It authenticates
//!   the same way `licensing.mp.microsoft.com` does (see
//!   [`crate::licensing::content`]): the *device* MSA token for
//!   `www.microsoft.com` goes in `Authorization`, the *user* MSA token for the
//!   same scope goes in the body as a beneficiary. An `XBL3.0` XSTS header is
//!   accepted too, but returns an empty `items` list for PC accounts.
//! * The PC Game Pass catalog is the public "sigl" list served by
//!   `catalog.gamepass.com` (no auth).

use serde::{Deserialize, Serialize};

use crate::models::live::ExchangeUserTokenOutcome;
use crate::models::secrets::Token;
use crate::models::soap;
use crate::tokens::TokenManager;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

const COLLECTIONS_QUERY: &str = "https://collections.mp.microsoft.com/v7.0/collections/query";
const MSA_SCOPE: &str = "www.microsoft.com";
const HOSTING_APP: &str = "{d6d5a677-0872-4ab0-9442-bb792fce85c5}";
const PAGE_SIZE: u32 = 100;
/// Safety stop for the continuation loop.
const MAX_PAGES: usize = 100;

/// "All PC Games" sigl: PC Game Pass including the EA Play PC titles.
/// (`fdd9e2a7-0fee-49f6-ad69-4354098401ff` is the same list without EA Play.)
pub const GAMEPASS_PC_ALL_GAMES_SIGL: &str = "609d944c-d395-4c0a-9ea4-e9f39b52c1ad";

#[derive(Debug, Clone, Serialize)]
pub struct OwnedProduct {
    /// Store BigId, e.g. `BQVQTL3PCH05`.
    pub product_id: String,
    pub acquisition: Acquisition,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Acquisition {
    /// Entitlement backed by an order (bought, or a bundle/edition containing it).
    Purchased,
    /// Entitlement granted through a recurring subscription (Game Pass / EA Play).
    GamePass,
    /// Anything else: free claims, promotional grants, ...
    Other,
}

impl Acquisition {
    fn rank(self) -> u8 {
        match self {
            Acquisition::Purchased => 2,
            Acquisition::GamePass => 1,
            Acquisition::Other => 0,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CollectionsQuery<'a> {
    beneficiaries: [Beneficiary<'a>; 1],
    market: &'a str,
    max_page_size: u32,
    validity_type: &'a str,
    product_kinds: [&'a str; 1],
    #[serde(skip_serializing_if = "Option::is_none")]
    continuation_token: Option<&'a str>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Beneficiary<'a> {
    identity_type: &'a str,
    identity_value: &'a str,
    local_ticket_reference: &'a str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CollectionsResponse {
    #[serde(default)]
    items: Vec<CollectionItem>,
    continuation_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CollectionItem {
    product_id: String,
    #[serde(default)]
    product_kind: String,
    #[serde(default)]
    status: String,
    order_id: Option<String>,
    recurrence_id: Option<String>,
}

impl CollectionItem {
    fn acquisition(&self) -> Acquisition {
        if self.order_id.as_deref().is_some_and(|s| !s.is_empty()) {
            Acquisition::Purchased
        } else if self.recurrence_id.as_deref().is_some_and(|s| !s.is_empty()) {
            Acquisition::GamePass
        } else {
            Acquisition::Other
        }
    }
}

/// Active `Game` entitlements of the signed-in user (DLC, durables and apps are
/// filtered out), deduplicated by product id. When a product is held through
/// several channels the strongest wins: purchased > game pass > other.
pub async fn get_owned_products(
    client: &reqwest::Client,
    tokens: &TokenManager,
) -> Result<Vec<OwnedProduct>, BoxError> {
    let (device_token, user_token, puid) = store_tokens(client, tokens).await?;

    let mut owned: Vec<OwnedProduct> = Vec::new();
    let mut continuation: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let query = CollectionsQuery {
            beneficiaries: [Beneficiary {
                identity_type: "Msa",
                identity_value: &user_token,
                local_ticket_reference: &puid,
            }],
            market: "US",
            max_page_size: PAGE_SIZE,
            validity_type: "Valid",
            product_kinds: ["Game"],
            continuation_token: continuation.as_deref(),
        };
        let page: CollectionsResponse = client
            .post(COLLECTIONS_QUERY)
            .header("Authorization", &device_token)
            .json(&query)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        for item in page.items {
            if item.product_kind != "Game" || item.status != "Active" {
                continue;
            }
            let acquisition = item.acquisition();
            match owned.iter_mut().find(|p| p.product_id == item.product_id) {
                Some(existing) if acquisition.rank() > existing.acquisition.rank() => {
                    existing.acquisition = acquisition;
                }
                Some(_) => {}
                None => owned.push(OwnedProduct {
                    product_id: item.product_id,
                    acquisition,
                }),
            }
        }

        match page.continuation_token {
            Some(token) if !token.is_empty() => continuation = Some(token),
            _ => return Ok(owned),
        }
    }
    tracing::warn!("collections query stopped after {MAX_PAGES} pages");
    Ok(owned)
}

/// Public PC Game Pass catalog product ids (no auth).
pub async fn get_gamepass_pc_product_ids(
    client: &reqwest::Client,
    market: &str,
) -> Result<Vec<String>, BoxError> {
    #[derive(Deserialize)]
    struct SiglEntry {
        // The first element is the list header (siglId/title/...) and has no `id`.
        id: Option<String>,
    }

    let url = url::Url::parse_with_params(
        "https://catalog.gamepass.com/sigls/v2",
        &[
            ("id", GAMEPASS_PC_ALL_GAMES_SIGL),
            ("language", "en-us"),
            ("market", market),
        ],
    )?;
    let entries: Vec<SiglEntry> = client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(entries.into_iter().filter_map(|e| e.id).collect())
}

/// Device token, user token (both MSA compact tickets for `www.microsoft.com`)
/// and the user's PUID, as the Store services expect them.
async fn store_tokens(
    client: &reqwest::Client,
    tokens: &TokenManager,
) -> Result<(String, String, String), BoxError> {
    let Token::Legacy(dev_token) = tokens.get_device_sts_token()? else {
        return Err("unsupported device STS token".into());
    };
    let Token::Legacy(user_sts) = tokens.get_user_sts_token()? else {
        return Err("unsupported user STS token".into());
    };
    let user = tokens.get_user()?;

    let device: Token = crate::api::live::exchange_device_token(
        client,
        dev_token.clone(),
        HOSTING_APP.to_string(),
        MSA_SCOPE.to_string(),
        Some(soap::PolicyReference::mbi_ssl()),
    )
    .await?
    .into();
    let Token::Compact(device) = device else {
        return Err("unexpected device token type".into());
    };

    let outcome = crate::api::live::exchange_user_token(
        client,
        user_sts,
        user.username,
        dev_token,
        None,
        Some("Silent".to_string()),
        HOSTING_APP.to_string(),
        &[(
            MSA_SCOPE.to_string(),
            Some(soap::PolicyReference::mbi_ssl()),
        )],
    )
    .await?;
    let user_token: Token = match outcome {
        ExchangeUserTokenOutcome::Issued(
            soap::BodyContent::RequestSecurityTokenResponseCollection(mut collection),
        ) if !collection.security_tokens.is_empty() => collection.security_tokens.remove(0).into(),
        ExchangeUserTokenOutcome::Issued(soap::BodyContent::RequestSecurityTokenResponse(
            token,
        )) => (*token).into(),
        _ => return Err("failed to exchange user token (sign in again?)".into()),
    };
    let Token::Compact(user_token) = user_token else {
        return Err("unexpected user token type".into());
    };

    Ok((device, user_token, user.puid))
}
