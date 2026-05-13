//! HTTP forwarding helpers for JSON-RPC to the Monad EL.

use reqwest::Client;

use crate::config::Config;

/// Forward arbitrary JSON-RPC body to a base URL (POST with application/json).
pub async fn forward_jsonrpc(
    client: &Client,
    base_url: &str,
    body: serde_json::Value,
    timeout: std::time::Duration,
) -> Result<reqwest::Response, reqwest::Error> {
    let url = base_url.trim_end_matches('/');
    client.post(url).json(&body).timeout(timeout).send().await
}

pub fn build_client(_config: &Config) -> Result<Client, reqwest::Error> {
    Client::builder().build()
}
