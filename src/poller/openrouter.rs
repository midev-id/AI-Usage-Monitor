use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use serde::Deserialize;

use super::{build_agent, PollError};
use crate::diagnose;
use crate::models::{CreditsSection, UsageData};

const CREDITS_URL: &str = "https://openrouter.ai/api/v1/credits";
const API_KEY_ENV: &str = "OPENROUTER_API_KEY";

#[derive(Deserialize)]
struct CreditsResponse {
    data: CreditsData,
}

#[derive(Deserialize)]
struct CreditsData {
    total_credits: f64,
    total_usage: f64,
}

/// OpenRouter is pay-as-you-go with no session or weekly rate-limit windows,
/// so the only meaningful figure is how much of the lifetime top-up has been
/// spent. `total_credits` is what the account has ever purchased (including
/// free grants); an account that has never topped up reports 0 and has
/// nothing meaningful to gauge, matching how Codex hides its credits bar
/// before a baseline exists.
pub(super) fn poll_openrouter() -> Result<UsageData, PollError> {
    let api_key = read_api_key().ok_or_else(|| {
        diagnose::log(format!(
            "OpenRouter usage poll failed: {API_KEY_ENV} is not set"
        ));
        PollError::NoCredentials
    })?;

    let response = fetch_credits(&api_key)?;
    Ok(usage_from_credits(response.data))
}

pub(super) fn credential_watch_snapshot(_all_sources: bool) -> Vec<String> {
    vec![match read_api_key() {
        Some(key) => {
            let mut hasher = DefaultHasher::new();
            key.hash(&mut hasher);
            format!("api_key|present|{}|{:x}", key.len(), hasher.finish())
        }
        None => "api_key|missing".to_string(),
    }]
}

fn fetch_credits(api_key: &str) -> Result<CreditsResponse, PollError> {
    let response = match build_agent()?
        .get(CREDITS_URL)
        .set("Authorization", &format!("Bearer {api_key}"))
        .set("Accept", "application/json")
        .call()
    {
        Ok(response) => response,
        Err(ureq::Error::Status(401 | 403, _)) => return Err(PollError::AuthRequired),
        Err(error) => {
            diagnose::log_error("OpenRouter credits request failed", error);
            return Err(PollError::RequestFailed);
        }
    };

    response.into_json().map_err(|error| {
        diagnose::log_error("unable to parse OpenRouter credits response", error);
        PollError::RequestFailed
    })
}

fn usage_from_credits(credits: CreditsData) -> UsageData {
    let mut data = UsageData::default();
    if credits.total_credits > 0.0 {
        let percentage = (credits.total_usage / credits.total_credits * 100.0).clamp(0.0, 100.0);
        data.credits = Some(CreditsSection {
            percentage,
            remaining: (credits.total_credits - credits.total_usage).max(0.0),
            total: credits.total_credits,
        });
    }
    data
}

fn read_api_key() -> Option<String> {
    std::env::var(API_KEY_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_topped_up_account_exposes_a_credits_gauge() {
        let data = usage_from_credits(CreditsData {
            total_credits: 50.0,
            total_usage: 12.5,
        });

        let credits = data.credits.expect("gauge");
        assert_eq!(credits.percentage, 25.0);
        assert_eq!(credits.remaining, 37.5);
        assert_eq!(credits.total, 50.0);
    }

    #[test]
    fn an_account_that_never_topped_up_has_no_gauge() {
        let data = usage_from_credits(CreditsData {
            total_credits: 0.0,
            total_usage: 0.0,
        });

        assert!(data.credits.is_none());
    }

    #[test]
    fn usage_past_the_purchased_amount_clamps_to_full() {
        let data = usage_from_credits(CreditsData {
            total_credits: 10.0,
            total_usage: 14.0,
        });

        let credits = data.credits.expect("gauge");
        assert_eq!(credits.percentage, 100.0);
        assert_eq!(credits.remaining, 0.0);
    }
}
