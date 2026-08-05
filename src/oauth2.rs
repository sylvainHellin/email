// OAuth2 Device Code Flow for Microsoft Entra ID (Exchange Online)
//
// Implements the device authorization grant for acquiring access tokens
// to use with IMAP (XOAUTH2) and SMTP (XOAUTH2) on Exchange Online.
//
// Token cache is stored encrypted at `<mailypoppins_data_dir>/tokens/<account>.enc`
// using the same machine-bound key as the secrets store (see `secrets.rs`).

use anyhow::{anyhow, Context, Result};
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// Microsoft Entra ID endpoints
// ---------------------------------------------------------------------------

fn device_code_url(tenant_id: &str) -> String {
    format!(
        "https://login.microsoftonline.com/{}/oauth2/v2.0/devicecode",
        tenant_id
    )
}

fn token_url(tenant_id: &str) -> String {
    format!(
        "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
        tenant_id
    )
}

/// Scopes required for IMAP + SMTP access via OAuth2.
pub const IMAP_SMTP_SCOPES: &str = "https://outlook.office365.com/IMAP.AccessAsUser.All \
                                    https://outlook.office365.com/SMTP.Send \
                                    offline_access";

/// Scopes required for Microsoft Graph API mail access.
pub const GRAPH_SCOPES: &str = "https://graph.microsoft.com/Mail.Read \
                                https://graph.microsoft.com/Mail.ReadWrite \
                                https://graph.microsoft.com/Mail.Send \
                                offline_access";

/// Return the appropriate OAuth2 scopes for a given auth method.
pub fn scopes_for_auth_method(method: &crate::config::AuthMethod) -> &'static str {
    match method {
        crate::config::AuthMethod::Graph => GRAPH_SCOPES,
        crate::config::AuthMethod::OAuth2 | crate::config::AuthMethod::Password => IMAP_SMTP_SCOPES,
    }
}

// ---------------------------------------------------------------------------
// Token cache
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TokenCache {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix timestamp when the access token expires.
    pub expires_at: u64,
}

impl TokenCache {
    pub fn is_expired(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // Consider expired 60 seconds early to avoid race conditions
        now >= self.expires_at.saturating_sub(60)
    }
}

/// Path to the encrypted token cache for `account_name`.
pub fn token_cache_path(account_name: &str) -> PathBuf {
    crate::config::tokens_dir().join(format!("{}.enc", account_name))
}

fn save_token_cache(account_name: &str, cache: &TokenCache) -> Result<()> {
    let dir = crate::config::tokens_dir();
    fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create token cache dir: {}", dir.display()))?;
    let path = token_cache_path(account_name);
    let json = serde_json::to_vec(cache).context("Failed to serialize token cache")?;
    let blob = crate::secrets::encrypt_blob(&json)
        .context("Failed to encrypt token cache")?;
    // Atomic write, created with 0600 from the start (no umask window).
    crate::secrets::write_secret_file_atomic(&path, &blob)
        .with_context(|| format!("Failed to write token cache: {}", path.display()))?;
    debug!("Token cache saved: {}", path.display());
    Ok(())
}

/// Load the cached token for `account_name`, or `None` if missing /
/// undecryptable / corrupt.
pub fn load_token_cache(account_name: &str) -> Option<TokenCache> {
    let path = token_cache_path(account_name);
    let bytes = fs::read(&path).ok()?;
    let plaintext = crate::secrets::decrypt_blob(&bytes).ok()?;
    serde_json::from_slice(&plaintext).ok()
}

// ---------------------------------------------------------------------------
// Device Code Flow response types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default = "default_interval")]
    interval: u64,
    #[serde(default = "default_expires_in")]
    expires_in: u64,
}

fn default_interval() -> u64 {
    5
}
fn default_expires_in() -> u64 {
    900
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
}

#[derive(Deserialize)]
struct TokenErrorResponse {
    error: String,
    error_description: Option<String>,
}

// ---------------------------------------------------------------------------
// Device Code Flow
// ---------------------------------------------------------------------------

/// Run the interactive device code flow. Prints the user code and verification URL,
/// then polls until the user completes sign-in or the flow expires.
pub async fn device_code_flow(
    client_id: &str,
    tenant_id: &str,
    account_name: &str,
    scopes: &str,
) -> Result<TokenCache> {
    let client = reqwest::Client::new();

    // Step 1: Request device code
    let resp = client
        .post(&device_code_url(tenant_id))
        .form(&[("client_id", client_id), ("scope", scopes)])
        .send()
        .await
        .context("Failed to request device code")?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("Device code request failed: {}", body));
    }

    let dc: DeviceCodeResponse = resp.json().await.context("Failed to parse device code response")?;

    // Step 2: Display instructions to user
    println!();
    println!("  To sign in, open a browser and go to:");
    println!();
    println!("    {}", dc.verification_uri);
    println!();
    println!("  Enter the code: {}", dc.user_code);
    println!();

    // Step 3: Poll for token
    let poll_interval = Duration::from_secs(dc.interval.max(5));
    let deadline = SystemTime::now() + Duration::from_secs(dc.expires_in);

    loop {
        tokio::time::sleep(poll_interval).await;

        if SystemTime::now() > deadline {
            return Err(anyhow!("Device code flow expired. Please try again."));
        }

        let resp = client
            .post(&token_url(tenant_id))
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", client_id),
                ("device_code", &dc.device_code),
            ])
            .send()
            .await
            .context("Failed to poll for token")?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();

        if status.is_success() {
            let token: TokenResponse =
                serde_json::from_str(&body).context("Failed to parse token response")?;

            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            let cache = TokenCache {
                access_token: token.access_token,
                refresh_token: token.refresh_token.unwrap_or_default(),
                expires_at: now + token.expires_in,
            };

            save_token_cache(account_name, &cache)?;
            info!("OAuth2 token acquired for account '{}'", account_name);
            return Ok(cache);
        }

        // Check error type
        if let Ok(err) = serde_json::from_str::<TokenErrorResponse>(&body) {
            match err.error.as_str() {
                "authorization_pending" => {
                    // User hasn't completed sign-in yet, keep polling
                    continue;
                }
                "slow_down" => {
                    // Back off by adding 5 seconds
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
                "expired_token" => {
                    return Err(anyhow!("Device code expired. Please try again."));
                }
                "authorization_declined" => {
                    return Err(anyhow!("Authorization was declined by the user."));
                }
                other => {
                    let desc = err.error_description.unwrap_or_default();
                    return Err(anyhow!("OAuth2 error ({}): {}", other, desc));
                }
            }
        } else {
            return Err(anyhow!("Unexpected token response (HTTP {}): {}", status, body));
        }
    }
}

// ---------------------------------------------------------------------------
// Token refresh
// ---------------------------------------------------------------------------

async fn refresh_token(
    client_id: &str,
    tenant_id: &str,
    refresh_token_value: &str,
    account_name: &str,
    scopes: &str,
) -> Result<TokenCache> {
    let client = reqwest::Client::new();

    let resp = client
        .post(&token_url(tenant_id))
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", refresh_token_value),
            ("scope", scopes),
        ])
        .send()
        .await
        .context("Failed to refresh OAuth2 token")?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!(
            "Token refresh failed (the refresh token may have expired -- run `mp config oauth2-login`): {}",
            body
        ));
    }

    let token: TokenResponse = resp.json().await.context("Failed to parse refresh response")?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let cache = TokenCache {
        access_token: token.access_token,
        refresh_token: token
            .refresh_token
            .unwrap_or_else(|| refresh_token_value.to_string()),
        expires_at: now + token.expires_in,
    };

    save_token_cache(account_name, &cache)?;
    info!("OAuth2 token refreshed for account '{}'", account_name);
    Ok(cache)
}

// ---------------------------------------------------------------------------
// Public API: load or refresh
// ---------------------------------------------------------------------------

/// Load a cached token, refresh if expired, or return an error if no cache exists.
/// For interactive token acquisition, use `device_code_flow()` instead.
pub async fn load_or_refresh_token(
    account_name: &str,
    client_id: &str,
    tenant_id: &str,
    scopes: &str,
) -> Result<String> {
    let cache = load_token_cache(account_name).ok_or_else(|| {
        anyhow!(
            "No OAuth2 token cached for account '{}'. Run `mp config oauth2-login {}` first.",
            account_name,
            account_name
        )
    })?;

    if !cache.is_expired() {
        debug!("Using cached OAuth2 token for '{}'", account_name);
        return Ok(cache.access_token);
    }

    if cache.refresh_token.is_empty() {
        return Err(anyhow!(
            "OAuth2 token expired and no refresh token available. Run `mp config oauth2-login {}`.",
            account_name
        ));
    }

    warn!(
        "OAuth2 token expired for '{}', refreshing...",
        account_name
    );
    let new_cache = refresh_token(client_id, tenant_id, &cache.refresh_token, account_name, scopes).await?;
    Ok(new_cache.access_token)
}

/// Blocking wrapper around `load_or_refresh_token` for use in config loading.
pub fn load_or_refresh_token_blocking(
    account_name: &str,
    client_id: &str,
    tenant_id: &str,
    scopes: &str,
) -> Result<String> {
    // Try to use existing tokio runtime first (e.g., when called from async context)
    let scopes = scopes.to_string();
    if let Ok(_handle) = tokio::runtime::Handle::try_current() {
        // We're inside an async runtime -- spawn a blocking task
        let scopes = scopes.clone();
        std::thread::scope(|s| {
            s.spawn(|| {
                let rt = tokio::runtime::Runtime::new()?;
                rt.block_on(load_or_refresh_token(account_name, client_id, tenant_id, &scopes))
            })
            .join()
            .map_err(|_| anyhow!("OAuth2 token refresh thread panicked"))?
        })
    } else {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(load_or_refresh_token(account_name, client_id, tenant_id, &scopes))
    }
}

// ---------------------------------------------------------------------------
// XOAUTH2 SASL string builder
// ---------------------------------------------------------------------------

/// Build the XOAUTH2 SASL string for IMAP/SMTP authentication.
/// Format: `base64("user=<email>\x01auth=Bearer <token>\x01\x01")`
pub fn build_xoauth2_string(username: &str, access_token: &str) -> String {
    use base64::Engine;
    let raw = format!("user={}\x01auth=Bearer {}\x01\x01", username, access_token);
    base64::engine::general_purpose::STANDARD.encode(raw.as_bytes())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_xoauth2_string() {
        let result = build_xoauth2_string("user@example.com", "ya29.token");
        // Decode and verify structure
        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD.decode(&result).unwrap();
        let decoded_str = String::from_utf8(decoded).unwrap();
        assert_eq!(decoded_str, "user=user@example.com\x01auth=Bearer ya29.token\x01\x01");
    }

    #[test]
    fn test_token_cache_expired() {
        let cache = TokenCache {
            access_token: "token".to_string(),
            refresh_token: "refresh".to_string(),
            expires_at: 0, // long expired
        };
        assert!(cache.is_expired());
    }

    #[test]
    fn test_token_cache_not_expired() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let cache = TokenCache {
            access_token: "token".to_string(),
            refresh_token: "refresh".to_string(),
            expires_at: now + 3600, // 1 hour from now
        };
        assert!(!cache.is_expired());
    }

    #[test]
    fn test_token_cache_almost_expired() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // Expires in 30 seconds -- should be considered expired (60s early margin)
        let cache = TokenCache {
            access_token: "token".to_string(),
            refresh_token: "refresh".to_string(),
            expires_at: now + 30,
        };
        assert!(cache.is_expired());
    }

    #[test]
    fn test_token_cache_serde_roundtrip() {
        let cache = TokenCache {
            access_token: "access".to_string(),
            refresh_token: "refresh".to_string(),
            expires_at: 1700000000,
        };
        let json = serde_json::to_string(&cache).unwrap();
        let back: TokenCache = serde_json::from_str(&json).unwrap();
        assert_eq!(back.access_token, "access");
        assert_eq!(back.refresh_token, "refresh");
        assert_eq!(back.expires_at, 1700000000);
    }

    #[test]
    fn test_device_code_url() {
        let url = device_code_url("my-tenant-id");
        assert_eq!(
            url,
            "https://login.microsoftonline.com/my-tenant-id/oauth2/v2.0/devicecode"
        );
    }

    #[test]
    fn test_token_url() {
        let url = token_url("my-tenant-id");
        assert_eq!(
            url,
            "https://login.microsoftonline.com/my-tenant-id/oauth2/v2.0/token"
        );
    }
}
