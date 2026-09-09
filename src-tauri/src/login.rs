//! In-app OAuth login: replicates `claude /login`'s Authorization Code + PKCE
//! flow so a new or expired account can be authorized from inside the app,
//! without a terminal. Spec reverse-engineered from the CLI binary — see
//! docs/usage-api.md -> "OAuth login / initial authorization".
//!
//! Uses the **manual-paste** redirect, not a loopback listener. A loopback
//! `http://localhost:<port>/callback` redirect passes validation and renders a
//! real consent screen, but Anthropic refuses it at the moment it would issue
//! the code: approving gives "Authorization failed / Invalid request format"
//! and the browser never comes back (verified 2026-09-08). The real client
//! builds BOTH URLs and falls back to the paste flow exactly when its loopback
//! listener gets nothing, so this is its own supported path, not a workaround.

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::RngCore;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::time::Duration;

const AUTHORIZE_URL: &str = "https://claude.com/cai/oauth/authorize";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
// Where the authorization code lands. Anthropic's page then shows a
// `<code>#<state>` string for the user to copy back into the app.
const MANUAL_REDIRECT_URL: &str = "https://platform.claude.com/oauth/code/callback";
// The profile the real client fetches right after every login (identity + plan
// fields the token response does not carry).
const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
// What the authorize request asks for: the real client's list, in its order,
// INCLUDING `org:create_api_key` — which the stored/refreshed set below does
// not have. Asking for a different set than the real client is one half of why
// claude.ai answered "Authorization failed" after a successful consent.
const AUTHORIZE_SCOPES: &[&str] = &[
    "org:create_api_key",
    "user:profile",
    "user:inference",
    "user:sessions:claude_code",
    "user:mcp_servers",
    "user:file_upload",
];
// Fallback for the stored `scopes` array when the exchange response omits
// `scope`: the real client's *refresh* set, which drops `org:create_api_key`
// and matches what CC actually has on disk for a logged-in account.
const STORED_SCOPES: &[&str] = &[
    "user:profile",
    "user:inference",
    "user:sessions:claude_code",
    "user:mcp_servers",
    "user:file_upload",
];
pub(crate) fn b64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub(crate) fn random_token(len: usize) -> String {
    let mut bytes = vec![0u8; len];
    rand::thread_rng().fill_bytes(&mut bytes);
    b64url(&bytes)
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn authorize_url(redirect_uri: &str, code_challenge: &str, state: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(AUTHORIZE_URL).context("parsing authorize URL constant")?;
    url.query_pairs_mut()
        // Unconditional in the real client — the loopback path included, despite
        // what this app's own docs used to claim. Omitting it is the other half
        // of the "Authorization failed" bug.
        .append_pair("code", "true")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", &AUTHORIZE_SCOPES.join(" "))
        .append_pair("code_challenge", code_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    Ok(url.to_string())
}

/// Look up a value by the first key present, trying each candidate in order —
/// used because the exact wire response casing for the *initial* login
/// exchange is not 100% wire-confirmed (unlike token refresh, which we've
/// verified live is snake_case), so we tolerate either.
fn get_str<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| v.get(*k)).and_then(|x| x.as_str())
}
fn get_i64(v: &Value, keys: &[&str]) -> Option<i64> {
    keys.iter().find_map(|k| v.get(*k)).and_then(|x| x.as_i64())
}

/// Pull the identity triple out of anything shaped like the token-exchange or
/// the profile response (an `account` object plus an `organization` one).
/// `None` when there is no account uuid — the id every switch keys off.
fn identity_from(v: &Value) -> Option<serde_json::Map<String, Value>> {
    let account = v.get("account");
    let organization = v.get("organization");
    let uuid = account.and_then(|a| get_str(a, &["uuid", "id", "account_uuid"]))?;
    let mut identity = serde_json::Map::new();
    identity.insert("accountUuid".into(), json!(uuid));
    if let Some(e) = account.and_then(|a| get_str(a, &["email_address", "emailAddress", "email"])) {
        identity.insert("emailAddress".into(), json!(e));
    }
    if let Some(o) = organization.and_then(|o| get_str(o, &["uuid", "id", "organization_uuid"])) {
        identity.insert("organizationUuid".into(), json!(o));
    }
    if let Some(n) = organization.and_then(|o| get_str(o, &["name", "organization_name"])) {
        identity.insert("organizationName".into(), json!(n));
    }
    // Descriptive extras, present on the profile response only. Without them a
    // freshly logged-in account is the "stub identity" switch warns about.
    if let Some(v) = account.and_then(|a| get_str(a, &["display_name"])) {
        identity.insert("displayName".into(), json!(v));
    }
    if let Some(v) = account.and_then(|a| get_str(a, &["full_name"])) {
        identity.insert("fullName".into(), json!(v));
    }
    if let Some(v) = account.and_then(|a| get_str(a, &["created_at"])) {
        identity.insert("accountCreatedAt".into(), json!(v));
    }
    if let Some(v) = organization.and_then(|o| get_str(o, &["organization_type"])) {
        identity.insert("organizationType".into(), json!(v));
    }
    if let Some(v) = organization.and_then(|o| get_str(o, &["rate_limit_tier"])) {
        identity.insert("organizationRateLimitTier".into(), json!(v));
    }
    if let Some(v) = organization.and_then(|o| get_str(o, &["billing_type"])) {
        identity.insert("billingType".into(), json!(v));
    }
    if let Some(v) = organization.and_then(|o| get_str(o, &["seat_tier"])) {
        identity.insert("seatTier".into(), json!(v));
    }
    Some(identity)
}

/// `organization.organization_type` -> the `subscriptionType` CC stores.
fn profile_subscription_type(profile: &Value) -> Option<String> {
    let org_type = get_str(profile.get("organization")?, &["organization_type"])?;
    Some(
        match org_type {
            "claude_max" => "max",
            "claude_pro" => "pro",
            "claude_enterprise" => "enterprise",
            "claude_team" => "team",
            _ => return None,
        }
        .to_string(),
    )
}

/// Best-effort profile fetch. A failure here must not sink an otherwise good
/// login, so every error path collapses to `None`.
async fn fetch_profile(access_token: &str) -> Option<Value> {
    let resp = reqwest::Client::new()
        .get(PROFILE_URL)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Content-Type", "application/json")
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<Value>().await.ok()
}

/// Exchange an authorization code for tokens + identity. Returns
/// `(oauth, identity)` already shaped the way `store.rs` expects: `oauth`
/// matches the stored `claudeAiOauth` object, `identity` matches a (possibly
/// partial) `oauthAccount` object.
async fn exchange_code(
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
    state: &str,
) -> Result<(Value, Value)> {
    let client = reqwest::Client::new();
    let resp = client
        .post(TOKEN_URL)
        .header("Content-Type", "application/json")
        .json(&json!({
            "grant_type": "authorization_code",
            "code": code,
            "state": state,
            "redirect_uri": redirect_uri,
            "client_id": CLIENT_ID,
            "code_verifier": code_verifier,
        }))
        .send()
        .await
        .context("token exchange request failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("token exchange failed ({status}): {body}"));
    }

    let body: Value = resp
        .json()
        .await
        .context("parsing the token exchange response")?;

    let access_token = get_str(&body, &["access_token", "accessToken"])
        .ok_or_else(|| anyhow!("token exchange response has no access token"))?;
    let refresh_token = get_str(&body, &["refresh_token", "refreshToken"])
        .ok_or_else(|| anyhow!("token exchange response has no refresh token"))?;
    let expires_at_ms = if let Some(expires_in) = get_i64(&body, &["expires_in"]) {
        now_ms() + expires_in * 1000
    } else if let Some(expires_at) = get_i64(&body, &["expiresAt", "expires_at"]) {
        expires_at
    } else {
        // Shouldn't happen; fall back to a conservative 1-hour lifetime rather
        // than failing outright, since a subsequent usage fetch will refresh
        // it anyway once it looks expired.
        now_ms() + 3600 * 1000
    };
    let scopes: Vec<String> = match body.get("scopes") {
        Some(Value::Array(arr)) => arr.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
        _ => match get_str(&body, &["scope"]) {
            Some(s) => s.split_whitespace().map(String::from).collect(),
            None => STORED_SCOPES.iter().map(|s| s.to_string()).collect(),
        },
    };
    let mut oauth = serde_json::Map::new();
    oauth.insert("accessToken".into(), json!(access_token));
    oauth.insert("refreshToken".into(), json!(refresh_token));
    oauth.insert("expiresAt".into(), json!(expires_at_ms));
    oauth.insert("scopes".into(), json!(scopes));
    // CC keeps this next to expiresAt; carry it when the server sends it, since
    // this blob is what a switch later installs as CC's own live credentials.
    if let Some(secs) = get_i64(&body, &["refresh_token_expires_in"]) {
        oauth.insert("refreshTokenExpiresAt".into(), json!(now_ms() + secs * 1000));
    }

    // Identity comes from /api/oauth/profile FIRST. Its `account.uuid` is the
    // same value Claude Code stores as `oauthAccount.accountUuid` (verified
    // live against a real account), and that is the id every switch and every
    // "is this account already saved?" check keys off. The token response is
    // only a gap-filler: its `account` is optional to begin with
    // (`e.account ? {...} : undefined` in the real client), and letting it win
    // made a re-login of an already-saved account ask for a new name.
    let profile = fetch_profile(access_token).await;
    let mut identity = profile.as_ref().and_then(identity_from).unwrap_or_default();
    if let Some(from_body) = identity_from(&body) {
        for (k, v) in from_body {
            identity.entry(k).or_insert(v);
        }
    }
    if !identity.contains_key("accountUuid") {
        return Err(anyhow!(
            "logged in, but could not read the account identity (the token response carried no account and the profile fetch failed) — nothing was saved"
        ));
    }

    // Plan fields: from the response when it has them, otherwise from the
    // profile, which is where the real client sources them.
    let subscription_type = get_str(&body, &["subscriptionType", "subscription_type"])
        .map(String::from)
        .or_else(|| profile.as_ref().and_then(profile_subscription_type));
    let rate_limit_tier = get_str(&body, &["rateLimitTier", "rate_limit_tier"])
        .map(String::from)
        .or_else(|| {
            profile.as_ref().and_then(|p| {
                get_str(p.get("organization")?, &["rate_limit_tier"]).map(String::from)
            })
        });
    if let Some(s) = subscription_type {
        oauth.insert("subscriptionType".into(), json!(s));
    }
    if let Some(r) = rate_limit_tier {
        oauth.insert("rateLimitTier".into(), json!(r));
    }

    Ok((Value::Object(oauth), Value::Object(identity)))
}

/// PKCE material for one in-flight login, held between `begin` and `complete`.
/// Cloned out of the app state rather than taken, so a mistyped paste can be
/// retried without restarting the browser flow.
#[derive(Clone)]
pub struct PendingAuth {
    verifier: String,
    state: String,
}

/// Start a login: build the authorize URL and hand it to the system browser.
/// The user approves there, and Anthropic's page then shows the `code#state`
/// string that `complete` takes. Returns the URL as well, so the caller can
/// offer it again if the browser failed to come up.
pub fn begin() -> Result<(String, PendingAuth)> {
    let verifier = random_token(32);
    let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
    // 32 bytes, matching the real client's `S() = d(R(32))`.
    let state = random_token(32);
    let url = authorize_url(MANUAL_REDIRECT_URL, &challenge, &state)?;

    tauri_plugin_opener::open_url(&url, None::<&str>)
        .map_err(|e| anyhow!("could not open the system browser: {e}"))?;

    Ok((url, PendingAuth { verifier, state }))
}

/// What the user pasted. Anthropic's page shows `<code>#<state>` (the real
/// client splits on '#' the same way), but people reasonably paste the whole
/// callback URL from the address bar instead, so accept that too. Returns the
/// code plus whatever state came with it, if any.
fn parse_pasted(pasted: &str) -> Result<(String, Option<String>)> {
    let pasted = pasted.trim();
    if pasted.is_empty() {
        return Err(anyhow!("no authorization code was pasted"));
    }
    if pasted.starts_with("http://") || pasted.starts_with("https://") {
        let url = reqwest::Url::parse(pasted).context("parsing the pasted callback URL")?;
        let mut code = None;
        let mut state = None;
        for (k, v) in url.query_pairs() {
            match k.as_ref() {
                "code" => code = Some(v.into_owned()),
                "state" => state = Some(v.into_owned()),
                _ => {}
            }
        }
        let code = code.ok_or_else(|| anyhow!("that link has no 'code' in it — copy the code the page shows"))?;
        return Ok((code, state));
    }
    match pasted.split_once('#') {
        Some((c, s)) => Ok((c.trim().to_string(), Some(s.trim().to_string()))),
        None => Ok((pasted.to_string(), None)),
    }
}

/// Finish a login from what the user pasted: verify it belongs to this attempt,
/// then exchange it for tokens + identity. Touches no file — the caller decides
/// where the result gets written (see `lib.rs` `login_submit_code`).
pub async fn complete(pasted: &str, pending: &PendingAuth) -> Result<(Value, Value)> {
    let (code, state) = parse_pasted(pasted)?;
    if let Some(state) = state.as_deref() {
        if state != pending.state {
            return Err(anyhow!(
                "that code belongs to a different login attempt — start Add account again and use the newest code"
            ));
        }
    }
    exchange_code(&code, &pending.verifier, MANUAL_REDIRECT_URL, &pending.state).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The authorize request must match the real client's parameter set. Both
    /// of these were missing and produced a claude.ai "Authorization failed"
    /// *after* a successful consent — the login page renders fine either way,
    /// so only the redirect step ever caught it.
    #[test]
    fn authorize_url_matches_the_real_clients_parameters() {
        let url = authorize_url(MANUAL_REDIRECT_URL, "chal", "st8").unwrap();
        let parsed = reqwest::Url::parse(&url).unwrap();
        let q: std::collections::HashMap<String, String> = parsed
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();

        assert_eq!(q.get("code").map(String::as_str), Some("true"));
        assert_eq!(
            q.get("scope").map(String::as_str),
            Some("org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload"),
        );
        assert_eq!(q.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(q.get("code_challenge_method").map(String::as_str), Some("S256"));
        assert_eq!(q.get("code_challenge").map(String::as_str), Some("chal"));
        assert_eq!(q.get("state").map(String::as_str), Some("st8"));
        assert_eq!(q.get("client_id").map(String::as_str), Some(CLIENT_ID));
        assert_eq!(
            q.get("redirect_uri").map(String::as_str),
            Some("https://platform.claude.com/oauth/code/callback"),
        );
    }

    /// `org:create_api_key` is asked for at authorize time but is NOT part of
    /// the set we fall back to storing/refreshing (the real client's refresh
    /// list drops it, and a logged-in CC has exactly these five on disk).
    #[test]
    fn stored_scope_fallback_drops_the_api_key_scope() {
        assert!(AUTHORIZE_SCOPES.contains(&"org:create_api_key"));
        assert!(!STORED_SCOPES.contains(&"org:create_api_key"));
        assert_eq!(STORED_SCOPES.len(), 5);
    }

    #[test]
    fn identity_comes_out_of_a_profile_shaped_response() {
        let v = json!({
            "account": { "uuid": "acc-1", "email_address": "a@b.c", "display_name": "A" },
            "organization": { "uuid": "org-1", "name": "Acme", "organization_type": "claude_max" }
        });
        let id = identity_from(&v).unwrap();
        assert_eq!(id.get("accountUuid").unwrap(), "acc-1");
        assert_eq!(id.get("emailAddress").unwrap(), "a@b.c");
        assert_eq!(id.get("organizationUuid").unwrap(), "org-1");
        assert_eq!(id.get("organizationName").unwrap(), "Acme");
    }

    /// No account uuid = nothing to key a switch off, so the caller must treat
    /// it as "no identity here" and go look elsewhere.
    /// The profile response spells these differently from the token response
    /// (`email` vs `email_address`) and carries the descriptive fields.
    #[test]
    fn identity_reads_the_profile_responses_own_spelling() {
        let profile = json!({
            "account": { "uuid": "acc-9", "email": "p@q.r", "display_name": "Dee", "full_name": "Dee Eff" },
            "organization": { "uuid": "org-9", "name": "Org", "rate_limit_tier": "tier_x" }
        });
        let id = identity_from(&profile).unwrap();
        assert_eq!(id.get("accountUuid").unwrap(), "acc-9");
        assert_eq!(id.get("emailAddress").unwrap(), "p@q.r");
        assert_eq!(id.get("displayName").unwrap(), "Dee");
        assert_eq!(id.get("fullName").unwrap(), "Dee Eff");
        assert_eq!(id.get("organizationRateLimitTier").unwrap(), "tier_x");
    }

    #[test]
    fn identity_is_none_without_an_account_uuid() {
        assert!(identity_from(&json!({ "organization": { "uuid": "org-1" } })).is_none());
        assert!(identity_from(&json!({ "account": { "email_address": "a@b.c" } })).is_none());
    }

    /// The page shows `code#state`, but a pasted address bar must work too —
    /// and a code from an older attempt must be refused, not exchanged.
    #[test]
    fn pasted_input_is_accepted_in_every_shape_the_user_might_supply() {
        assert_eq!(parse_pasted("abc#xyz").unwrap(), ("abc".into(), Some("xyz".into())));
        assert_eq!(parse_pasted("  abc#xyz  ").unwrap(), ("abc".into(), Some("xyz".into())));
        assert_eq!(parse_pasted("abc").unwrap(), ("abc".into(), None));
        assert_eq!(
            parse_pasted("https://platform.claude.com/oauth/code/callback?code=abc&state=xyz").unwrap(),
            ("abc".into(), Some("xyz".into())),
        );
        assert!(parse_pasted("").is_err());
        assert!(parse_pasted("   ").is_err());
        assert!(parse_pasted("https://example.com/nope").is_err());
    }

    #[test]
    fn subscription_type_maps_the_organization_type() {
        let of = |t: &str| {
            profile_subscription_type(&json!({ "organization": { "organization_type": t } }))
        };
        assert_eq!(of("claude_max").as_deref(), Some("max"));
        assert_eq!(of("claude_pro").as_deref(), Some("pro"));
        assert_eq!(of("claude_enterprise").as_deref(), Some("enterprise"));
        assert_eq!(of("claude_team").as_deref(), Some("team"));
        assert_eq!(of("something_new"), None);
        assert_eq!(profile_subscription_type(&json!({})), None);
    }
}
