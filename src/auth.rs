//! OAuth 2.0 three-legged flow against LinkedIn.
//!
//! Two entry points on purpose: the loopback listener (the good path, it needs
//! `http://127.0.0.1:<port>/callback` registered on the app) and `--url <pasted>`
//! (the fallback, when LinkedIn insists on an https redirect: register any https
//! URL that 404s, let the browser fail, paste the address bar back in).
//!
//! LinkedIn issues a 60-day access token and a 365-day refresh token that
//! rotates on every use and does not extend. One re-auth per year is expected.

use crate::api::urlencode;
use crate::config::Config;
use crate::keychain;
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub expires_in: i64,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub refresh_token_expires_in: Option<i64>,
    #[serde(default)]
    pub scope: Option<String>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Meta {
    pub sub: String,
    pub name: String,
    pub scope: String,
    pub obtained_at: String,
    pub expires_at: String,
    pub refresh_expires_at: String,
}

pub const SCOPES: &str = "openid profile w_member_social";

pub fn random_state() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mixed = nanos ^ ((std::process::id() as u128) << 32);
    format!("{:x}", mixed.wrapping_mul(6364136223846793005))
}

pub fn authorize_url(cfg: &Config, state: &str) -> String {
    format!(
        "https://www.linkedin.com/oauth/v2/authorization?response_type=code&client_id={}\
         &redirect_uri={}&state={}&scope={}",
        urlencode(&cfg.client_id),
        urlencode(&cfg.redirect_uri()),
        urlencode(state),
        urlencode(SCOPES)
    )
}

/// Block on a single loopback request until the code arrives or we time out.
pub fn wait_for_code(port: u16, expect_state: &str, timeout: Duration) -> Result<String> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!("cannot bind loopback port {port} (is `mk-feed auth` already running?)"))?;
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + timeout;

    loop {
        if Instant::now() > deadline {
            anyhow::bail!("timed out waiting for the redirect after {}s", timeout.as_secs());
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                let mut buf = [0u8; 8192];
                let read = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..read]).to_string();
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("")
                    .to_string();

                let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
                let mut code = None;
                let mut state = None;
                let mut error = None;
                for pair in query.split('&') {
                    let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                    let v = urldecode(v);
                    match k {
                        "code" => code = Some(v),
                        "state" => state = Some(v),
                        "error" => error = Some(v),
                        "error_description" => error = Some(v),
                        _ => {}
                    }
                }

                let (status, body) = match (&code, &error) {
                    (_, Some(err)) => ("400 Bad Request", format!("LinkedIn returned: {err}")),
                    (Some(_), None) if state.as_deref() != Some(expect_state) => {
                        ("400 Bad Request", "state mismatch — retry `mk-feed auth`".to_string())
                    }
                    (Some(_), None) => (
                        "200 OK",
                        "Authorized. Close this tab and return to the terminal.".to_string(),
                    ),
                    _ => ("400 Bad Request", "no code in the redirect".to_string()),
                };
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
                let _ = stream.flush();

                if let Some(err) = error {
                    anyhow::bail!("authorization failed: {err}");
                }
                if let Some(code) = code {
                    return Ok(code);
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(120));
            }
            Err(e) => return Err(e).context("accepting the loopback redirect"),
        }
    }
}

pub fn code_from_url(url: &str) -> Result<String> {
    let query = url
        .split_once('?')
        .map(|(_, q)| q)
        .context("pasted URL has no query string")?;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k == "code" {
            return Ok(urldecode(v));
        }
        if k == "error" || k == "error_description" {
            anyhow::bail!("LinkedIn returned an error instead of a code: {}", urldecode(v));
        }
    }
    anyhow::bail!("no `code=` in the pasted URL")
}

pub fn urldecode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn client_secret() -> Result<String> {
    keychain::get(keychain::ACC_SECRET).context(
        "no client secret in the Keychain — store it once with:\n  \
         security add-generic-password -U -a linkedin.client_secret -s mk-feed -w '<secret>'",
    )
}

pub fn exchange_code(cfg: &Config, code: &str) -> Result<TokenResponse> {
    let secret = client_secret()?;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let form = format!(
        "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&client_secret={}",
        urlencode(code),
        urlencode(&cfg.redirect_uri()),
        urlencode(&cfg.client_id),
        urlencode(&secret)
    );
    let response = client
        .post("https://www.linkedin.com/oauth/v2/accessToken")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form)
        .send()
        .context("token endpoint unreachable")?;
    let status = response.status();
    let text = response.text().unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("token exchange failed ({status}): {text}");
    }
    serde_json::from_str(&text).with_context(|| format!("unexpected token response: {text}"))
}

pub fn refresh(cfg: &Config) -> Result<TokenResponse> {
    let secret = client_secret()?;
    let refresh_token =
        keychain::get(keychain::ACC_REFRESH).context("no refresh token — run `mk-feed auth`")?;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let form = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}&client_secret={}",
        urlencode(&refresh_token),
        urlencode(&cfg.client_id),
        urlencode(&secret)
    );
    let response = client
        .post("https://www.linkedin.com/oauth/v2/accessToken")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form)
        .send()
        .context("token endpoint unreachable")?;
    let status = response.status();
    let text = response.text().unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("refresh failed ({status}): {text}");
    }
    serde_json::from_str(&text).with_context(|| format!("unexpected refresh response: {text}"))
}

pub fn store_tokens(cfg: &Config, tokens: &TokenResponse) -> Result<Meta> {
    let now = chrono::Utc::now();
    let expires_at = now + chrono::Duration::seconds(tokens.expires_in.max(3600));
    let refresh_expires_at = now
        + chrono::Duration::seconds(tokens.refresh_token_expires_in.unwrap_or(365 * 24 * 3600));

    keychain::set(keychain::ACC_TOKEN, &tokens.access_token)?;
    if let Some(refresh) = &tokens.refresh_token {
        keychain::set(keychain::ACC_REFRESH, refresh)?;
    }

    let mut meta: Meta = keychain::get(keychain::ACC_META)
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    meta.scope = tokens.scope.clone().unwrap_or_else(|| SCOPES.to_string());
    meta.obtained_at = now.to_rfc3339();
    meta.expires_at = expires_at.to_rfc3339();
    meta.refresh_expires_at = refresh_expires_at.to_rfc3339();
    if meta.sub.is_empty() {
        if let Ok((sub, name)) = userinfo(&tokens.access_token) {
            meta.sub = sub;
            meta.name = name;
        }
    }
    keychain::set(keychain::ACC_META, &serde_json::to_string(&meta)?)?;
    let _ = cfg;
    Ok(meta)
}

pub fn load_meta() -> Meta {
    keychain::get(keychain::ACC_META)
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn userinfo(access_token: &str) -> Result<(String, String)> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;
    let response = client
        .get("https://api.linkedin.com/v2/userinfo")
        .bearer_auth(access_token)
        .send()
        .context("userinfo unreachable")?;
    let status = response.status();
    let body: serde_json::Value = response.json().unwrap_or_else(|_| json!({}));
    if !status.is_success() {
        anyhow::bail!("userinfo failed ({status}): {body}");
    }
    let sub = body
        .get("sub")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let name = body
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    Ok((sub, name))
}

/// Access token, refreshed transparently if it is close to expiring.
pub fn access_token(cfg: &Config) -> Result<String> {
    let meta = load_meta();
    let token = keychain::get(keychain::ACC_TOKEN).context("no access token — run `mk-feed auth`")?;
    let expiring = chrono::DateTime::parse_from_rfc3339(&meta.expires_at)
        .map(|at| at.with_timezone(&chrono::Utc) < chrono::Utc::now() + chrono::Duration::minutes(2))
        .unwrap_or(true);
    if !expiring {
        return Ok(token);
    }
    let tokens = refresh(cfg)?;
    store_tokens(cfg, &tokens)?;
    Ok(tokens.access_token)
}
