//! OAuth with the ChatGPT account, the same way the Codex CLI does it.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::Value;
use url::Url;

use crate::crypto::{random_bytes, sha256};

const ISSUER: &str = "https://auth.openai.com";
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const SCOPE: &str = "openid profile email offline_access api.connectors.read api.connectors.invoke";
/// The redirect that the OAuth client allows. In a remote setup the browser cannot open it; the
/// admin copies the URL from the address bar instead.
pub const PKCE_REDIRECT: &str = "http://127.0.0.1:1455/auth/callback";
const DEVICE_REDIRECT: &str = "https://auth.openai.com/deviceauth/callback";
pub const DEVICE_VERIFICATION_URL: &str = "https://auth.openai.com/codex/device";
pub const ORIGINATOR: &str = "codex_cli_rs";
const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("the sign-in server answered {status}: {detail}")]
    Rejected { status: StatusCode, detail: String },
    #[error("cannot reach the sign-in server: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("unexpected answer from the sign-in server: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone)]
pub struct TokenSet {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
}

/// Data about the account from the id token.
#[derive(Debug, Clone)]
pub struct Identity {
    pub account_id: String,
    pub email: Option<String>,
    pub plan_type: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Tokens

/// Reads the payload of a JWT. The token comes from the token endpoint over TLS, so the
/// signature is not checked here.
fn jwt_payload(token: &str) -> Result<Value, OAuthError> {
    let payload = token
        .split('.')
        .nth(1)
        .ok_or_else(|| OAuthError::Invalid("the token is not a JWT".into()))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .map_err(|_| OAuthError::Invalid("the token payload is not base64".into()))?;
    serde_json::from_slice(&bytes).map_err(|_| OAuthError::Invalid("the token payload is not JSON".into()))
}

pub fn identity(id_token: &str) -> Result<Identity, OAuthError> {
    let claims = jwt_payload(id_token)?;
    let auth = &claims["https://api.openai.com/auth"];
    let account_id = auth["chatgpt_account_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| OAuthError::Invalid("the id token has no ChatGPT account id".into()))?;
    Ok(Identity {
        account_id: account_id.to_owned(),
        email: claims["email"].as_str().map(str::to_owned),
        plan_type: auth["chatgpt_plan_type"].as_str().map(str::to_owned),
    })
}

/// Expiry of the access token (unix seconds), from its `exp` claim.
pub fn expires_at(access_token: &str) -> Option<i64> {
    jwt_payload(access_token).ok()?["exp"].as_i64()
}

#[derive(Deserialize)]
struct TokenResponse {
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
}

async fn error_detail(response: reqwest::Response) -> OAuthError {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let detail: String = body.chars().take(300).collect();
    OAuthError::Rejected { status, detail }
}

/// Exchanges an authorization code for tokens.
pub async fn exchange_code(
    http: &Client,
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
) -> Result<TokenSet, OAuthError> {
    let response = http
        .post(format!("{ISSUER}/oauth/token"))
        .timeout(TIMEOUT)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", CLIENT_ID),
            ("code_verifier", code_verifier),
        ])
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(error_detail(response).await);
    }
    let tokens: TokenResponse = response.json().await?;
    match (tokens.id_token, tokens.access_token, tokens.refresh_token) {
        (Some(id_token), Some(access_token), Some(refresh_token)) => Ok(TokenSet {
            id_token,
            access_token,
            refresh_token,
        }),
        _ => Err(OAuthError::Invalid("the token answer misses a token".into())),
    }
}

/// Result of a refresh. Missing tokens in the answer keep their old value.
pub struct Refreshed {
    pub id_token: Option<String>,
    pub access_token: String,
    pub refresh_token: Option<String>,
}

pub enum RefreshError {
    /// The refresh token cannot work any more. The admin must link the account again.
    Permanent(String),
    /// Try again later.
    Temporary(String),
}

pub async fn refresh(http: &Client, refresh_token: &str, timeout: Duration) -> Result<Refreshed, RefreshError> {
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "client_id": CLIENT_ID,
        "refresh_token": refresh_token,
    });
    let response = http
        .post(format!("{ISSUER}/oauth/token"))
        .timeout(timeout)
        .json(&body)
        .send()
        .await
        .map_err(|err| RefreshError::Temporary(format!("cannot reach the sign-in server: {err}")))?;
    let status = response.status();
    if status.is_success() {
        let tokens: TokenResponse = response
            .json()
            .await
            .map_err(|err| RefreshError::Temporary(format!("unexpected refresh answer: {err}")))?;
        let access_token = tokens
            .access_token
            .ok_or_else(|| RefreshError::Temporary("the refresh answer has no access token".into()))?;
        return Ok(Refreshed {
            id_token: tokens.id_token,
            access_token,
            refresh_token: tokens.refresh_token,
        });
    }
    let text = response.text().await.unwrap_or_default();
    let json: Value = serde_json::from_str(&text).unwrap_or_default();
    let code = json["error"]["code"]
        .as_str()
        .or_else(|| json["error"].as_str())
        .or_else(|| json["code"].as_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let message = format!(
        "refresh failed with {status}: {}",
        text.chars().take(300).collect::<String>()
    );
    // Same classification as the Codex CLI.
    let permanent = status == StatusCode::UNAUTHORIZED
        || (status == StatusCode::BAD_REQUEST && code == "invalid_grant")
        || matches!(
            code.as_str(),
            "refresh_token_expired" | "refresh_token_reused" | "refresh_token_invalidated"
        );
    Err(if permanent {
        RefreshError::Permanent(message)
    } else {
        RefreshError::Temporary(message)
    })
}

// ---------------------------------------------------------------------------------------------
// Device code flow

pub struct DeviceCode {
    pub device_auth_id: String,
    pub user_code: String,
    pub interval: Duration,
}

pub async fn request_device_code(http: &Client) -> Result<DeviceCode, OAuthError> {
    let response = http
        .post(format!("{ISSUER}/api/accounts/deviceauth/usercode"))
        .timeout(TIMEOUT)
        .json(&serde_json::json!({ "client_id": CLIENT_ID }))
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(error_detail(response).await);
    }
    let body: Value = response.json().await?;
    let text = |key: &str| body[key].as_str().map(str::to_owned);
    let interval = body["interval"]
        .as_str()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .or_else(|| body["interval"].as_u64())
        .unwrap_or(5)
        .clamp(1, 60);
    Ok(DeviceCode {
        device_auth_id: text("device_auth_id").ok_or_else(|| OAuthError::Invalid("no device_auth_id".into()))?,
        user_code: text("user_code")
            .or_else(|| text("usercode"))
            .ok_or_else(|| OAuthError::Invalid("no user_code".into()))?,
        interval: Duration::from_secs(interval),
    })
}

/// One poll. `Ok(None)` means the user has not finished yet.
pub async fn poll_device_code(http: &Client, device: &DeviceCode) -> Result<Option<TokenSet>, OAuthError> {
    let response = http
        .post(format!("{ISSUER}/api/accounts/deviceauth/token"))
        .timeout(TIMEOUT)
        .json(&serde_json::json!({
            "device_auth_id": device.device_auth_id,
            "user_code": device.user_code,
        }))
        .send()
        .await?;
    let status = response.status();
    if status == StatusCode::FORBIDDEN || status == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(error_detail(response).await);
    }
    let body: Value = response.json().await?;
    let field = |key: &str| {
        body[key]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| OAuthError::Invalid(format!("the device answer has no {key}")))
    };
    let code = field("authorization_code")?;
    let verifier = field("code_verifier")?;
    exchange_code(http, &code, &verifier, DEVICE_REDIRECT).await.map(Some)
}

// ---------------------------------------------------------------------------------------------
// PKCE flow

pub struct Pkce {
    pub verifier: String,
    pub state: String,
    pub authorize_url: String,
}

pub fn start_pkce() -> Pkce {
    let verifier = URL_SAFE_NO_PAD.encode(random_bytes(64));
    let challenge = URL_SAFE_NO_PAD.encode(sha256(verifier.as_bytes()));
    let state = URL_SAFE_NO_PAD.encode(random_bytes(32));
    let mut url = Url::parse(&format!("{ISSUER}/oauth/authorize")).expect("valid authorize URL");
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", PKCE_REDIRECT)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("state", &state)
        .append_pair("originator", ORIGINATOR);
    Pkce {
        verifier,
        state,
        authorize_url: url.into(),
    }
}

/// Reads `code` and `state` from the redirect URL that the admin pasted.
pub fn parse_callback(pasted: &str) -> Result<(String, String), String> {
    let url = Url::parse(pasted.trim()).map_err(|_| "Paste the full address from the browser.".to_owned())?;
    let param = |name: &str| url.query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.into_owned());
    if let Some(error) = param("error") {
        return Err(format!("The sign-in was not completed: {error}."));
    }
    match (param("code"), param("state")) {
        (Some(code), Some(state)) => Ok((code, state)),
        _ => Err("The address has no sign-in code. Paste the full address from the browser.".into()),
    }
}
