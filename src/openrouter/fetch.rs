//! OpenRouter fetch — combines `/api/v1/credits` and `/api/v1/key` under
//! the shared cache + flock primitives.

use std::time::Duration;

use crate::cache::{Cache, MAX_STALE, acquire_lock_async};
use crate::error::{AppError, Result};
use crate::usage::OpenRouterSnapshot;

use super::types::{
    CreditsData, KeyData, ModelData, ModelEndpointsEnvelope, OrEnvelope, RankingsEnvelope, combine,
    lowest_endpoint_prices, weekly_leaderboard,
};

pub const BASE_URL: &str = "https://openrouter.ai/api/v1";
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const LOCK_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
pub struct Endpoints {
    pub credits: String,
    pub key: String,
    pub rankings: String,
    pub models: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            credits: format!("{BASE_URL}/credits"),
            key: format!("{BASE_URL}/key"),
            rankings: format!("{BASE_URL}/datasets/rankings-daily"),
            models: format!("{BASE_URL}/models"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FetchOutcome {
    pub snapshot: OpenRouterSnapshot,
    pub stale: bool,
    pub last_error: Option<(u16, String)>,
    pub cache_age: Option<Duration>,
}

/// Cache-aware fetch. Mirrors `anthropic::fetch::fetch_snapshot` semantics:
/// fresh cache short-circuits; on failure, fall back to cache + mark stale.
pub async fn fetch_snapshot(
    client: &reqwest::Client,
    api_key: &str,
    cache: &Cache,
    endpoints: &Endpoints,
    cache_ttl: Duration,
) -> Result<FetchOutcome> {
    cache.ensure_dir()?;
    let _lock = acquire_lock_async(&cache.lock_path(), LOCK_TIMEOUT).await?;

    if let Some(bytes) = cache.fresh_payload(cache_ttl)?
        && let Ok(outcome) = reuse_cache(bytes, cache, false)
    {
        return Ok(outcome);
    }
    // Corrupt fresh cache: fall through to live fetch rather than return a
    // fabricated zero-credit snapshot.

    match fetch_live(client, endpoints, api_key).await {
        Ok((credits, key, rankings, models)) => {
            let mut snap = combine(credits, key);
            if let (Some(rankings), Some(models)) = (rankings, models) {
                snap.leaderboard_as_of = Some(rankings.meta.as_of.clone());
                snap.weekly_leaderboard = weekly_leaderboard(rankings, models, 10);
                enrich_floor_prices(
                    client,
                    api_key,
                    &endpoints.models,
                    &mut snap.weekly_leaderboard,
                )
                .await;
            }
            // Serialize back to JSON for the cache.
            let cache_repr = serde_json::json!({
                "snapshot": serde_repr(&snap),
            });
            let bytes = serde_json::to_vec(&cache_repr)?;
            cache.write_payload(&bytes)?;
            Ok(FetchOutcome {
                snapshot: snap,
                stale: false,
                last_error: None,
                cache_age: Some(Duration::ZERO),
            })
        }
        Err(e) if e.is_transient() => fallback_silent(cache),
        Err(AppError::Http { status, body }) => {
            cache.mark_stale();
            let last_error = Some(cache.write_last_error(status, &body));
            fallback_with_error(cache, last_error)
        }
        Err(e) => {
            cache.mark_stale();
            let last_error = Some(cache.write_last_error(0, &e.to_string()));
            fallback_with_error(cache, last_error)
        }
    }
}

fn fallback_silent(cache: &Cache) -> Result<FetchOutcome> {
    let Some(bytes) = cache.fallback_payload(MAX_STALE)? else {
        return Err(AppError::Transport(
            "openrouter: no cache and network unreachable".into(),
        ));
    };
    reuse_cache(bytes, cache, true)
}

fn fallback_with_error(cache: &Cache, last_error: Option<(u16, String)>) -> Result<FetchOutcome> {
    let Some(bytes) = cache.fallback_payload(MAX_STALE)? else {
        return Err(AppError::Other("openrouter: no usable cache".into()));
    };
    let mut outcome = reuse_cache(bytes, cache, true)?;
    outcome.last_error = last_error;
    Ok(outcome)
}

fn reuse_cache(bytes: Vec<u8>, cache: &Cache, stale: bool) -> Result<FetchOutcome> {
    let snap = parse_cache(&bytes)?;
    Ok(FetchOutcome {
        snapshot: snap,
        stale,
        last_error: cache.read_last_error(),
        cache_age: cache.payload_age(),
    })
}

/// Cached money is required, not optional: a truncated or half-written payload
/// must be refetched rather than rendered as $0.00 with a free-tier badge.
/// `limit`/`limit_remaining` stay optional — the API itself returns them null.
fn parse_cache(bytes: &[u8]) -> Result<OpenRouterSnapshot> {
    let v: serde_json::Value = serde_json::from_slice(bytes)?;
    let s = v
        .get("snapshot")
        .ok_or_else(|| AppError::Schema("openrouter cache missing 'snapshot' field".into()))?;
    let money = |name: &str| -> Result<f64> {
        let n = s
            .get(name)
            .and_then(serde_json::Value::as_f64)
            .ok_or_else(|| AppError::Schema(format!("openrouter cache missing '{name}'")))?;
        if n.is_finite() && n >= 0.0 {
            Ok(n)
        } else {
            Err(AppError::Schema(format!(
                "openrouter cache '{name}' is not finite and non-negative"
            )))
        }
    };
    let optional_money = |name: &str, nonnegative: bool| -> Result<Option<f64>> {
        match s.get(name) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(value) => {
                let number = value.as_f64().ok_or_else(|| {
                    AppError::Schema(format!("openrouter cache '{name}' is not numeric or null"))
                })?;
                if number.is_finite() && (!nonnegative || number >= 0.0) {
                    Ok(Some(number))
                } else {
                    Err(AppError::Schema(format!(
                        "openrouter cache '{name}' is outside its valid range"
                    )))
                }
            }
        }
    };
    Ok(OpenRouterSnapshot {
        label: s
            .get("label")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| AppError::Schema("openrouter cache missing 'label'".into()))?
            .to_string(),
        total_credits: money("total_credits")?,
        total_usage: money("total_usage")?,
        usage_daily: money("usage_daily")?,
        usage_weekly: money("usage_weekly")?,
        usage_monthly: money("usage_monthly")?,
        is_free_tier: s["is_free_tier"]
            .as_bool()
            .ok_or_else(|| AppError::Schema("openrouter cache missing 'is_free_tier'".into()))?,
        limit: optional_money("limit", true)?,
        limit_remaining: optional_money("limit_remaining", false)?,
        weekly_leaderboard: s
            .get("weekly_leaderboard")
            .cloned()
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default(),
        leaderboard_as_of: s
            .get("leaderboard_as_of")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
    })
}

fn serde_repr(snap: &OpenRouterSnapshot) -> serde_json::Value {
    serde_json::json!({
        "label": snap.label,
        "total_credits": snap.total_credits,
        "total_usage": snap.total_usage,
        "usage_daily": snap.usage_daily,
        "usage_weekly": snap.usage_weekly,
        "usage_monthly": snap.usage_monthly,
        "is_free_tier": snap.is_free_tier,
        "limit": snap.limit,
        "limit_remaining": snap.limit_remaining,
        "weekly_leaderboard": snap.weekly_leaderboard,
        "leaderboard_as_of": snap.leaderboard_as_of,
    })
}

async fn fetch_live(
    client: &reqwest::Client,
    endpoints: &Endpoints,
    api_key: &str,
) -> Result<(
    CreditsData,
    KeyData,
    Option<RankingsEnvelope>,
    Option<Vec<ModelData>>,
)> {
    // Fetch in parallel.
    let credits_fut = fetch_one::<CreditsData>(client, &endpoints.credits, api_key);
    let key_fut = fetch_one::<KeyData>(client, &endpoints.key, api_key);
    let rankings_fut = fetch_json::<RankingsEnvelope>(client, &endpoints.rankings, api_key);
    let models_fut = fetch_one::<Vec<ModelData>>(client, &endpoints.models, api_key);
    let (credits, key, rankings, models) =
        tokio::join!(credits_fut, key_fut, rankings_fut, models_fut);
    Ok((credits?, key?, rankings.ok(), models.ok()))
}

async fn fetch_json<T: for<'de> serde::Deserialize<'de>>(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
) -> Result<T> {
    let resp = tokio::time::timeout(
        HTTP_TIMEOUT,
        client
            .get(url)
            .header("Authorization", format!("Bearer {api_key}"))
            .send(),
    )
    .await
    .map_err(|_| AppError::Transport(format!("openrouter timeout: {url}")))??;
    let status = resp.status();
    let bytes = crate::vendor::read_body_capped(resp, crate::vendor::MAX_BODY_BYTES).await?;
    if !status.is_success() {
        return Err(AppError::Http {
            status: status.as_u16(),
            body: String::from_utf8_lossy(&bytes).chars().take(200).collect(),
        });
    }
    serde_json::from_slice(&bytes).map_err(|e| AppError::Schema(format!("openrouter {url}: {e}")))
}

/// Replace catalog/list prices with the lowest currently advertised provider
/// prices, matching the decision a `:floor` caller is trying to make. Endpoint
/// failures are deliberately per-model and optional: one provider page must
/// never hide the leaderboard or account balance.
async fn enrich_floor_prices(
    client: &reqwest::Client,
    api_key: &str,
    models_base: &str,
    rows: &mut [crate::usage::OpenRouterModelRank],
) {
    let mut tasks = tokio::task::JoinSet::new();
    for (index, row) in rows.iter().enumerate() {
        if row.model_id.ends_with(":free")
            || matches!(
                (row.prompt_price, row.completion_price),
                (Some(0.0), Some(0.0))
            )
        {
            continue;
        }
        let client = client.clone();
        let api_key = api_key.to_string();
        let url = format!("{models_base}/{}/endpoints", row.model_id);
        tasks.spawn(async move {
            (
                index,
                fetch_json::<ModelEndpointsEnvelope>(&client, &url, &api_key).await,
            )
        });
    }
    while let Some(result) = tasks.join_next().await {
        let Ok((index, Ok(envelope))) = result else {
            continue;
        };
        let (prompt, completion) = lowest_endpoint_prices(&envelope.data);
        if prompt.is_some() {
            rows[index].prompt_price = prompt;
        }
        if completion.is_some() {
            rows[index].completion_price = completion;
        }
    }
}

async fn fetch_one<T: for<'de> serde::Deserialize<'de>>(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
) -> Result<T> {
    let resp = tokio::time::timeout(
        HTTP_TIMEOUT,
        client
            .get(url)
            .header("Authorization", format!("Bearer {api_key}"))
            .send(),
    )
    .await
    .map_err(|_| AppError::Transport(format!("openrouter timeout: {url}")))??;

    let status = resp.status();
    let bytes = crate::vendor::read_body_capped(resp, crate::vendor::MAX_BODY_BYTES).await?;

    if !status.is_success() {
        let body = String::from_utf8_lossy(&bytes).chars().take(200).collect();
        return Err(AppError::Http {
            status: status.as_u16(),
            body,
        });
    }
    let env: OrEnvelope<T> = serde_json::from_slice(&bytes)
        .map_err(|e| AppError::Schema(format!("openrouter {url}: {e}")))?;
    Ok(env.data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn cache_fixture() -> (TempDir, Cache) {
        let td = TempDir::new().unwrap();
        let cache = Cache::at(td.path().join("openrouter"));
        cache.ensure_dir().unwrap();
        (td, cache)
    }

    #[tokio::test]
    async fn live_fetch_combines_both_endpoints() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/v1/credits")
            .with_status(200)
            .with_body(r#"{"data":{"total_credits":100.0,"total_usage":25.5}}"#)
            .create_async()
            .await;
        server
            .mock("GET", "/api/v1/key")
            .with_status(200)
            .with_body(
                r#"{"data":{"label":"prod","limit":50.0,"limit_remaining":24.5,
                "usage":25.5,"usage_daily":1.0,"usage_weekly":7.0,"usage_monthly":25.5,
                "is_free_tier":false}}"#,
            )
            .create_async()
            .await;
        server
            .mock("GET", "/api/v1/datasets/rankings-daily")
            .with_status(200)
            .with_body(
                r#"{"data":[{"date":"2026-09-18","model_permaslug":"acme/alpha-202609","total_tokens":"123456789"}],"meta":{"end_date":"2026-09-18","as_of":"2026-09-19T01:00:00Z"}}"#,
            )
            .create_async()
            .await;
        server
            .mock("GET", "/api/v1/models")
            .with_status(200)
            .with_body(
                r#"{"data":[{"id":"acme/alpha","canonical_slug":"acme/alpha-202609","name":"Acme: Alpha","pricing":{"prompt":"0.000001","completion":"0.000002"}}]}"#,
            )
            .create_async()
            .await;

        let (_td, cache) = cache_fixture();
        let client = reqwest::Client::new();
        let endpoints = Endpoints {
            credits: format!("{}/api/v1/credits", server.url()),
            key: format!("{}/api/v1/key", server.url()),
            rankings: format!("{}/api/v1/datasets/rankings-daily", server.url()),
            models: format!("{}/api/v1/models", server.url()),
        };
        let out = fetch_snapshot(
            &client,
            "sk-or-test",
            &cache,
            &endpoints,
            Duration::from_secs(0),
        )
        .await
        .unwrap();
        assert_eq!(out.snapshot.total_credits, 100.0);
        assert_eq!(out.snapshot.total_usage, 25.5);
        assert!((out.snapshot.balance() - 74.5).abs() < 1e-9);
        assert_eq!(out.snapshot.label, "OpenRouter — prod");
        assert_eq!(out.snapshot.weekly_leaderboard[0].name, "Alpha");
        assert_eq!(
            out.snapshot.leaderboard_as_of.as_deref(),
            Some("2026-09-19T01:00:00Z")
        );
        assert!(!out.stale);
    }

    #[tokio::test]
    async fn http_error_falls_back_to_cache_when_present() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/v1/credits")
            .with_status(401)
            .with_body(r#"{"error":"unauthorized"}"#)
            .create_async()
            .await;
        server
            .mock("GET", "/api/v1/key")
            .with_status(401)
            .with_body(r#"{"error":"unauthorized"}"#)
            .create_async()
            .await;

        let (_td, cache) = cache_fixture();
        // Seed cache with a "snapshot" repr.
        let seed = serde_json::json!({
            "snapshot": {
                "label":"OpenRouter — seed","total_credits": 50.0,
                "total_usage": 10.0,"usage_daily":1.0,"usage_weekly":3.0,
                "usage_monthly":10.0,"is_free_tier":false,
                "limit":null,"limit_remaining":null
            }
        });
        cache.write_payload(seed.to_string().as_bytes()).unwrap();

        let client = reqwest::Client::new();
        let endpoints = Endpoints {
            credits: format!("{}/api/v1/credits", server.url()),
            key: format!("{}/api/v1/key", server.url()),
            rankings: format!("{}/api/v1/datasets/rankings-daily", server.url()),
            models: format!("{}/api/v1/models", server.url()),
        };
        let out = fetch_snapshot(&client, "k", &cache, &endpoints, Duration::from_secs(0))
            .await
            .unwrap();
        assert!(out.stale);
        assert_eq!(out.snapshot.label, "OpenRouter — seed");
        assert_eq!(out.last_error.as_ref().map(|(c, _)| *c), Some(401));
    }
}
