//! Wire types for OpenRouter's `/api/v1/credits` and `/api/v1/key`.
//!
//! Both endpoints wrap their payload in `{ "data": { ... } }`, hence the
//! generic [`OrEnvelope`] wrapper.

use std::collections::HashMap;

use serde::Deserialize;

use crate::usage::{OpenRouterModelRank, OpenRouterSnapshot};

/// Wrapper used by all OpenRouter v1 endpoints.
#[derive(Debug, Clone, Deserialize)]
pub struct OrEnvelope<T> {
    pub data: T,
}

/// `GET /api/v1/credits` — total_credits and total_usage, both USD doubles.
#[derive(Debug, Clone, Deserialize)]
pub struct CreditsData {
    #[serde(deserialize_with = "de_nonnegative_finite")]
    pub total_credits: f64,
    #[serde(deserialize_with = "de_nonnegative_finite")]
    pub total_usage: f64,
}

/// `GET /api/v1/key` — per-key usage and free-tier flag.
#[derive(Debug, Clone, Deserialize)]
pub struct KeyData {
    #[serde(default)]
    pub label: String,
    #[serde(default, deserialize_with = "de_opt_nonnegative_finite")]
    pub limit: Option<f64>,
    #[serde(default, deserialize_with = "de_opt_finite")]
    pub limit_remaining: Option<f64>,
    #[serde(deserialize_with = "de_nonnegative_finite")]
    pub usage_daily: f64,
    #[serde(deserialize_with = "de_nonnegative_finite")]
    pub usage_weekly: f64,
    #[serde(deserialize_with = "de_nonnegative_finite")]
    pub usage_monthly: f64,
    pub is_free_tier: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RankingRow {
    pub date: String,
    pub model_permaslug: String,
    #[serde(deserialize_with = "de_u64_string")]
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RankingsMeta {
    pub end_date: String,
    pub as_of: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RankingsEnvelope {
    pub data: Vec<RankingRow>,
    pub meta: RankingsMeta,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelData {
    pub id: String,
    #[serde(default)]
    pub canonical_slug: String,
    pub name: String,
    pub pricing: ModelPricing,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelPricing {
    #[serde(default, deserialize_with = "de_opt_price")]
    pub prompt: Option<f64>,
    #[serde(default, deserialize_with = "de_opt_price")]
    pub completion: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelEndpointsEnvelope {
    pub data: ModelEndpointsData,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelEndpointsData {
    #[serde(default)]
    pub endpoints: Vec<ModelEndpoint>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelEndpoint {
    pub pricing: ModelPricing,
}

pub fn lowest_endpoint_prices(data: &ModelEndpointsData) -> (Option<f64>, Option<f64>) {
    let lowest = |price: fn(&ModelPricing) -> Option<f64>| {
        data.endpoints
            .iter()
            .filter_map(|endpoint| price(&endpoint.pricing))
            .min_by(f64::total_cmp)
    };
    (lowest(|p| p.prompt), lowest(|p| p.completion))
}

fn checked_finite<E: serde::de::Error>(value: f64) -> Result<f64, E> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(E::custom("money value is not finite"))
    }
}

fn checked_nonnegative<E: serde::de::Error>(value: f64) -> Result<f64, E> {
    let value = checked_finite(value)?;
    if value >= 0.0 {
        Ok(value)
    } else {
        Err(E::custom("money value cannot be negative"))
    }
}

fn de_nonnegative_finite<'de, D>(d: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    checked_nonnegative(f64::deserialize(d)?)
}

fn de_opt_nonnegative_finite<'de, D>(d: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<f64>::deserialize(d)?
        .map(checked_nonnegative)
        .transpose()
}

fn de_opt_finite<'de, D>(d: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<f64>::deserialize(d)?
        .map(checked_finite)
        .transpose()
}

fn de_u64_string<'de, D>(d: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(d)?;
    value.parse().map_err(serde::de::Error::custom)
}

fn de_opt_price<'de, D>(d: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // OpenRouter uses "-1" for router products whose price depends on the
    // model selected at request time. That is "not a fixed price", not schema
    // drift, and one such catalog row must not discard the entire leaderboard.
    Ok(Option::<String>::deserialize(d)?
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0))
}

/// Aggregate daily rows over the seven-day window ending at `meta.end_date`,
/// then enrich the top rows with display names and current model pricing.
pub fn weekly_leaderboard(
    rankings: RankingsEnvelope,
    models: Vec<ModelData>,
    limit: usize,
) -> Vec<OpenRouterModelRank> {
    let Ok(end) = chrono::NaiveDate::parse_from_str(&rankings.meta.end_date, "%Y-%m-%d") else {
        return Vec::new();
    };
    let start = end - chrono::Duration::days(6);
    let mut totals: HashMap<String, u64> = HashMap::new();
    for row in rankings.data {
        let Ok(date) = chrono::NaiveDate::parse_from_str(&row.date, "%Y-%m-%d") else {
            continue;
        };
        if row.model_permaslug != "other" && (start..=end).contains(&date) {
            let total = totals.entry(row.model_permaslug).or_default();
            *total = total.saturating_add(row.total_tokens);
        }
    }
    let mut totals: Vec<_> = totals.into_iter().collect();
    totals.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let mut model_by_slug = HashMap::new();
    for model in models {
        model_by_slug.insert(model.id.clone(), model.clone());
        if !model.canonical_slug.is_empty() {
            // Several variants share the same canonical slug. Preserve the
            // variant suffix in the lookup key so `:free`/`:batch` can never
            // overwrite the default paid model (or one another).
            let canonical_key = model
                .id
                .rsplit('/')
                .next()
                .and_then(|tail| tail.split_once(':'))
                .map_or_else(
                    || model.canonical_slug.clone(),
                    |(_, variant)| format!("{}:{variant}", model.canonical_slug),
                );
            model_by_slug.entry(canonical_key).or_insert(model);
        }
    }
    totals
        .into_iter()
        .take(limit)
        .enumerate()
        .map(|(index, (model_id, total_tokens))| {
            let model = model_by_slug.get(&model_id);
            OpenRouterModelRank {
                rank: (index + 1) as u16,
                name: model.map_or_else(
                    || model_id.clone(),
                    |m| {
                        // `/models` prefixes display names with the provider
                        // (`OpenAI: …`, `DeepSeek: …`), while the rankings UI
                        // shows the model title alone. Match that public view.
                        m.name
                            .split_once(": ")
                            .map_or_else(|| m.name.clone(), |(_, name)| name.to_string())
                    },
                ),
                prompt_price: model.and_then(|m| m.pricing.prompt),
                completion_price: model.and_then(|m| m.pricing.completion),
                model_id: model.map_or(model_id, |m| m.id.clone()),
                total_tokens,
            }
        })
        .collect()
}

/// Combine the two endpoint responses into the canonical snapshot.
pub fn combine(credits: CreditsData, key: KeyData) -> OpenRouterSnapshot {
    let label = if key.label.is_empty() {
        "OpenRouter".to_string()
    } else {
        format!("OpenRouter — {}", key.label)
    };
    OpenRouterSnapshot {
        label,
        total_credits: credits.total_credits,
        total_usage: credits.total_usage,
        usage_daily: key.usage_daily,
        usage_weekly: key.usage_weekly,
        usage_monthly: key.usage_monthly,
        is_free_tier: key.is_free_tier,
        limit: key.limit,
        limit_remaining: key.limit_remaining,
        weekly_leaderboard: Vec::new(),
        leaderboard_as_of: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_credits_envelope() {
        let raw = r#"{"data":{"total_credits":100.0,"total_usage":25.5}}"#;
        let env: OrEnvelope<CreditsData> = serde_json::from_str(raw).unwrap();
        assert_eq!(env.data.total_credits, 100.0);
        assert_eq!(env.data.total_usage, 25.5);
    }

    #[test]
    fn parses_key_envelope_with_nulls() {
        let raw = r#"{"data":{
            "label":"my-key",
            "limit":null,"limit_remaining":null,
            "usage":12.34,"usage_daily":1.0,"usage_weekly":3.0,"usage_monthly":12.0,
            "is_free_tier":false
        }}"#;
        let env: OrEnvelope<KeyData> = serde_json::from_str(raw).unwrap();
        assert_eq!(env.data.label, "my-key");
        assert!(env.data.limit.is_none());
        assert_eq!(env.data.usage_monthly, 12.0);
        assert!(!env.data.is_free_tier);
    }

    #[test]
    fn combine_builds_snapshot() {
        let c = CreditsData {
            total_credits: 100.0,
            total_usage: 30.0,
        };
        let k = KeyData {
            label: "key-A".into(),
            limit: Some(50.0),
            limit_remaining: Some(20.0),
            usage_daily: 1.0,
            usage_weekly: 5.0,
            usage_monthly: 30.0,
            is_free_tier: false,
        };
        let snap = combine(c, k);
        assert_eq!(snap.label, "OpenRouter — key-A");
        assert!((snap.balance() - 70.0).abs() < 1e-9);
        assert_eq!(snap.consumed_pct(), 30);
        assert_eq!(snap.usage_monthly, 30.0);
    }

    #[test]
    fn combine_with_empty_label() {
        let snap = combine(
            CreditsData {
                total_credits: 0.0,
                total_usage: 0.0,
            },
            KeyData {
                label: String::new(),
                limit: None,
                limit_remaining: None,
                usage_daily: 0.0,
                usage_weekly: 0.0,
                usage_monthly: 0.0,
                is_free_tier: false,
            },
        );
        assert_eq!(snap.label, "OpenRouter");
    }

    #[test]
    fn missing_required_money_does_not_deserialize_as_zero() {
        assert!(serde_json::from_str::<OrEnvelope<CreditsData>>(r#"{"data":{}}"#).is_err());
        assert!(
            serde_json::from_str::<OrEnvelope<KeyData>>(
                r#"{"data":{"label":"key","is_free_tier":false}}"#
            )
            .is_err()
        );
    }

    #[test]
    fn invalid_money_values_are_schema_drift() {
        for total in ["-1", "1e400", "true", r#""zero""#] {
            let raw = format!(r#"{{"data":{{"total_credits":{total},"total_usage":0}}}}"#);
            assert!(
                serde_json::from_str::<OrEnvelope<CreditsData>>(&raw).is_err(),
                "{raw}"
            );
        }
    }

    #[test]
    fn consumed_pct_handles_zero_credits() {
        let s = OpenRouterSnapshot {
            label: "x".into(),
            total_credits: 0.0,
            total_usage: 5.0,
            usage_daily: 0.0,
            usage_weekly: 0.0,
            usage_monthly: 0.0,
            is_free_tier: true,
            limit: None,
            limit_remaining: None,
            weekly_leaderboard: Vec::new(),
            leaderboard_as_of: None,
        };
        assert_eq!(s.consumed_pct(), 0);
    }

    #[test]
    fn weekly_ranking_uses_last_seven_complete_days_and_enriches_prices() {
        let rankings: RankingsEnvelope = serde_json::from_str(
            r#"{"data":[
                {"date":"2026-09-11","model_permaslug":"old/model","total_tokens":"9999"},
                {"date":"2026-09-12","model_permaslug":"acme/alpha-202609","total_tokens":"100"},
                {"date":"2026-09-18","model_permaslug":"acme/alpha-202609","total_tokens":"250"},
                {"date":"2026-09-18","model_permaslug":"acme/beta","total_tokens":"300"},
                {"date":"2026-09-18","model_permaslug":"other","total_tokens":"999999"}
            ],"meta":{"end_date":"2026-09-18","as_of":"2026-09-19T01:00:00Z"}}"#,
        )
        .unwrap();
        let models: OrEnvelope<Vec<ModelData>> = serde_json::from_str(
            r#"{"data":[
                {"id":"acme/alpha","canonical_slug":"acme/alpha-202609","name":"Acme: Alpha","pricing":{"prompt":"0.000001","completion":"0.000002"}},
                {"id":"acme/beta","canonical_slug":"acme/beta","name":"Acme: Beta","pricing":{"prompt":"0","completion":"0"}}
            ]}"#,
        )
        .unwrap();

        let rows = weekly_leaderboard(rankings, models.data, 5);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "Alpha");
        assert_eq!(rows[0].total_tokens, 350);
        assert_eq!(rows[0].prompt_price, Some(0.000001));
        assert_eq!(rows[1].model_id, "acme/beta");
    }

    #[test]
    fn variable_router_price_does_not_reject_the_model_catalog() {
        let models: OrEnvelope<Vec<ModelData>> = serde_json::from_str(
            r#"{"data":[
                {"id":"openrouter/auto","canonical_slug":"openrouter/auto","name":"Auto Router","pricing":{"prompt":"-1","completion":"-1"}},
                {"id":"acme/fixed","canonical_slug":"acme/fixed","name":"Fixed","pricing":{"prompt":"0.000001","completion":"0.000002"}}
            ]}"#,
        )
        .unwrap();

        assert_eq!(models.data.len(), 2);
        assert_eq!(models.data[0].pricing.prompt, None);
        assert_eq!(models.data[0].pricing.completion, None);
        assert_eq!(models.data[1].pricing.prompt, Some(0.000001));
    }

    #[test]
    fn canonical_slug_keeps_paid_batch_and_free_variants_distinct() {
        let rankings: RankingsEnvelope = serde_json::from_str(
            r#"{"data":[
                {"date":"2026-09-18","model_permaslug":"deepseek/model-20260731","total_tokens":"300"},
                {"date":"2026-09-18","model_permaslug":"deepseek/model-20260731:free","total_tokens":"200"},
                {"date":"2026-09-18","model_permaslug":"deepseek/model-20260731:batch","total_tokens":"100"}
            ],"meta":{"end_date":"2026-09-18","as_of":"2026-09-19T01:00:00Z"}}"#,
        )
        .unwrap();
        let models: OrEnvelope<Vec<ModelData>> = serde_json::from_str(
            r#"{"data":[
                {"id":"deepseek/model","canonical_slug":"deepseek/model-20260731","name":"Paid","pricing":{"prompt":"0.000001","completion":"0.000002"}},
                {"id":"deepseek/model:batch","canonical_slug":"deepseek/model-20260731","name":"Batch","pricing":{"prompt":"0.0000005","completion":"0.000001"}},
                {"id":"deepseek/model:free","canonical_slug":"deepseek/model-20260731","name":"Free","pricing":{"prompt":"0","completion":"0"}}
            ]}"#,
        )
        .unwrap();

        let rows = weekly_leaderboard(rankings, models.data, 10);
        assert_eq!(rows[0].name, "Paid");
        assert_eq!(rows[0].model_id, "deepseek/model");
        assert_eq!(rows[0].prompt_price, Some(0.000001));
        assert_eq!(rows[1].name, "Free");
        assert_eq!(rows[1].model_id, "deepseek/model:free");
        assert_eq!(rows[1].prompt_price, Some(0.0));
        assert_eq!(rows[2].name, "Batch");
        assert_eq!(rows[2].model_id, "deepseek/model:batch");
        assert_eq!(rows[2].prompt_price, Some(0.0000005));
    }

    #[test]
    fn floor_prices_take_the_lowest_value_across_provider_endpoints() {
        let envelope: ModelEndpointsEnvelope = serde_json::from_str(
            r#"{"data":{"endpoints":[
                {"pricing":{"prompt":"0.00000006","completion":"0.00000018"}},
                {"pricing":{"prompt":"0.00000004752","completion":"0.00000014256"}},
                {"pricing":{"prompt":"0.00000004796","completion":"0.00000014388"}}
            ]}}"#,
        )
        .unwrap();

        assert_eq!(
            lowest_endpoint_prices(&envelope.data),
            (Some(0.00000004752), Some(0.00000014256))
        );
    }
}
