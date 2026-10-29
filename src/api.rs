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

/// LinkedIn's `commentary` is not plain text: it is a "little format" where a
/// handful of characters are syntax. An unescaped "(" starts something the
/// parser cannot finish, and the platform **silently truncates the post at that
/// point** — measured with three probes on 2026-10-28: 229 of 1348 characters
/// arrived, then 17 of 327, while square brackets came through untouched.
///
/// The escape character is a backslash. This list mirrors what Postiz escapes
/// (`fixText` in its LinkedIn provider, AGPL — read, then verified against the
/// live API, not copied).
pub fn escape_little_format(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 24);
    for ch in text.chars() {
        match ch {
            '\\' | '<' | '>' | '[' | ']' | '(' | ')' | '{' | '}' | '*' | '_' | '~' | '|' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
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

/// Tests and dry runs must never reach the network. This is a hard gate, not a
/// convention: a sandboxed config with a live Keychain is exactly how a test
/// published to a real account once.
fn offline() -> bool {
    std::env::var_os("MK_FEED_OFFLINE").is_some()
}

pub fn create_post(
    client: &reqwest::blocking::Client,
    access_token: &str,
    author_urn: &str,
    commentary: &str,
    visibility: &str,
    api_version: &str,
) -> Result<String, (SendError, bool)> {
    if offline() {
        return Err((
            SendError::Rejected(
                "MK_FEED_OFFLINE is set: network disabled, nothing was sent".into(),
            ),
            false,
        ));
    }

    // The bool is "auth may be stale" — a 401 means refresh and retry once,
    // which is safe because a 401 never published anything.
    let payload = json!({
        "author": author_urn,
        "commentary": escape_little_format(commentary),
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

/// Delete a post. `w_member_social` allows it, and it is the only way to undo a
/// test post without touching the browser.
pub fn delete_post(
    client: &reqwest::blocking::Client,
    access_token: &str,
    urn: &str,
    api_version: &str,
) -> Result<(), (SendError, bool)> {
    if offline() {
        return Err((
            SendError::Rejected("MK_FEED_OFFLINE is set: nothing was deleted".into()),
            false,
        ));
    }
    let encoded = urlencode(urn);
    let response = client
        .delete(format!("https://api.linkedin.com/rest/posts/{encoded}"))
        .header("Authorization", format!("Bearer {access_token}"))
        .header("X-Restli-Protocol-Version", "2.0.0")
        .header("LinkedIn-Version", api_version)
        .send();

    match response {
        Ok(r) if r.status().as_u16() == 204 || r.status().is_success() => Ok(()),
        Ok(r) if r.status().as_u16() == 401 => {
            Err((SendError::Rejected("401".into()), true))
        }
        Ok(r) if r.status().is_client_error() => {
            Err((SendError::Rejected(format!("{}", r.status())), false))
        }
        Ok(r) => Err((SendError::Unknown(format!("{}", r.status())), false)),
        Err(e) => Err((SendError::Unknown(format!("network: {e}")), false)),
    }
}

pub fn public_url(urn: &str) -> String {
    format!("https://www.linkedin.com/feed/update/{urn}/")
}

#[cfg(test)]
mod tests {
    use super::escape_little_format;

    /// Measured 2026-10-28: an unescaped "(" made LinkedIn keep 229 of 1348
    /// characters and discard the rest. The escape set comes from Postiz's
    /// `fixText`; this test pins the behaviour so a refactor cannot quietly
    /// reintroduce the truncation.
    #[test]
    fn linkedin_syntax_is_escaped() {
        let raw = "Antes: siete contenedores (Postgres, Redis) y **nada** [sic] 3 veces.";
        let escaped = escape_little_format(raw);
        assert_eq!(
            escaped,
            "Antes: siete contenedores \\(Postgres, Redis\\) y \\*\\*nada\\*\\* \\[sic\\] 3 veces."
        );
        // Nothing else answers: removing the escape character must give the
        // original text back, byte for byte.
        assert_eq!(escaped.replace('\\', ""), raw);
    }

    #[test]
    fn plain_text_is_untouched() {
        let raw = "Un scheduler que reintenta es un scheduler que publica dos veces";
        assert_eq!(escape_little_format(raw), raw);
    }
}
