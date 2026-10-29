//! The only two things this tool asks of LinkedIn: publish a post, and tell me
//! who I am. No read scope exists here on purpose — see `SendError::Unknown`.

use anyhow::Result;
use serde_json::json;
use std::time::Duration;

pub fn urlencode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            b' ' => out.push_str("%20"),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// The two failure kinds that matter, because they demand different actions.
#[derive(Debug)]
pub enum SendError {
    /// LinkedIn answered and refused. Nothing was published. Safe to retry
    /// after fixing the payload.
    Rejected(String),
    /// We do not know whether it published. There is no idempotency key and no
    /// read scope to check, so a retry can double-post. A human must look.
    Unknown(String),
}

pub fn http() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(45))
        .build()?)
}

pub fn create_post(
    client: &reqwest::blocking::Client,
    access_token: &str,
    author_urn: &str,
    commentary: &str,
    visibility: &str,
    api_version: &str,
) -> Result<String, (SendError, bool)> {
    // The bool is "auth may be stale" — a 401 means refresh and retry once,
    // which is safe because a 401 never published anything.
    let payload = json!({
        "author": author_urn,
        "commentary": commentary,
        "visibility": visibility,
        "distribution": {
            "feedDistribution": "MAIN_FEED",
            "targetEntities": [],
            "thirdPartyDistributionChannels": []
        },
        "lifecycleState": "PUBLISHED",
        "isReshareDisabledByAuthor": false
    });

    let response = client
        .post("https://api.linkedin.com/rest/posts")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Content-Type", "application/json")
        .header("X-Restli-Protocol-Version", "2.0.0")
        .header("LinkedIn-Version", api_version)
        .json(&payload)
        .send();

    let response = match response {
        Ok(r) => r,
        Err(e) => {
            return Err((
                SendError::Unknown(format!("network error before a response: {e}")),
                false,
            ))
        }
    };

    let status = response.status();
    let restli_id = response
        .headers()
        .get("x-restli-id")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_string());
    let body = response.text().unwrap_or_default();

    if status.as_u16() == 201 || status.as_u16() == 200 {
        return match restli_id {
            Some(id) => Ok(id),
            None => Err((
                SendError::Unknown("201 with no x-restli-id header".into()),
                false,
            )),
        };
    }

    if status.as_u16() == 401 {
        return Err((SendError::Rejected(format!("401 {body}")), true));
    }
    // A definite refusal from the API: nothing was published.
    if status.is_client_error() && status.as_u16() != 408 && status.as_u16() != 429 {
        return Err((SendError::Rejected(format!("{status}: {body}")), false));
    }
    Err((
        SendError::Unknown(format!("{status}: {body}")),
        false,
    ))
}

pub fn public_url(urn: &str) -> String {
    format!("https://www.linkedin.com/feed/update/{urn}/")
}
