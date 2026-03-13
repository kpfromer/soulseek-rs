use crate::Error;
use governor::{Quota, RateLimiter, clock::DefaultClock, state::InMemoryState, state::direct::NotKeyed};
use reqwest::Client;
use serde::Deserialize;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::OnceLock;

type DirectRateLimiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock>;

static RATE_LIMITER: OnceLock<Arc<DirectRateLimiter>> = OnceLock::new();

fn rate_limiter() -> &'static Arc<DirectRateLimiter> {
    RATE_LIMITER.get_or_init(|| {
        Arc::new(RateLimiter::direct(Quota::per_second(
            NonZeroU32::new(1).unwrap(),
        )))
    })
}

#[derive(Debug, Deserialize)]
pub struct Response {
    pub results: Vec<ResultItem>,
    #[allow(dead_code)]
    pub status: String,
}

#[derive(Debug, Deserialize)]
pub struct ResultItem {
    #[allow(dead_code)]
    pub id: String,
    #[serde(default)]
    pub recordings: Vec<Recording>,
    pub score: f64,
}

#[derive(Debug, Deserialize)]
pub struct Recording {
    pub id: String,
    pub duration: Option<f64>,
}

pub async fn lookup(
    client: &Client,
    api_key: &str,
    fingerprint: &str,
    duration: u32,
) -> Result<Response, Error> {
    rate_limiter().until_ready().await;

    let url = format!(
        "https://api.acoustid.org/v2/lookup?client={}&meta=recordings&duration={}&fingerprint={}",
        api_key, duration, fingerprint
    );

    client
        .get(&url)
        .send()
        .await
        .map_err(|e| Error::AcoustId(e.to_string()))?
        .json::<Response>()
        .await
        .map_err(|e| Error::AcoustId(e.to_string()))
}
