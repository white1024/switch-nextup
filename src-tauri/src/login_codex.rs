//! In-app ChatGPT / Codex OAuth login: replicates `codex login`'s Authorization
//! Code + PKCE flow so a Codex account can be authorized from inside the app.
//! Spec reverse-engineered from the `openai/codex` source (`codex-rs/login/`)
//! and the codex binary installed on this machine, which agree — see
//! docs/usage-api.md -> "ChatGPT / Codex usage & auth API".
//!
//! Unlike the Claude flow ([`crate::login`], which has to make the user paste a
//! code), this is a real **loopback** login with nothing to copy. That works
//! here for one specific reason: Codex's redirect_uri carries a **fixed**
//! registered port, so `http://localhost:1455/auth/callback` is exactly the
//! value OpenAI has on file. Claude Code's client uses an ephemeral port, and
//! Anthropic refuses that redirect at the moment it would issue the code.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::net::TcpListener as StdTcpListener;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::login::{b64url, random_token};
use crate::provider::{self, Provider};

const ISSUER: &str = "https://auth.openai.com";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const SCOPES: &str = "openid profile email offline_access api.connectors.read api.connectors.invoke";
const ORIGINATOR: &str = "codex_cli_rs";
// Fixed, because the redirect_uri is registered with the port in it. 1457 is
// the real client's own fallback for "1455 is busy".
const DEFAULT_PORT: u16 = 1455;
const FALLBACK_PORT: u16 = 1457;
const CALLBACK_PATH: &str = "/auth/callback";
// Long enough to log in and pick an account, short enough that a forgotten
// window doesn't hold the port forever.
const CALLBACK_TIMEOUT_SECS: u64 = 300;

/// Bind the registered port, falling back the way the real client does. Both
/// being busy is the one failure mode a fixed port brings, so say plainly what
/// to do about it rather than surfacing a raw AddrInUse.
fn bind_loopback() -> Result<(StdTcpListener, u16)> {
    for port in [DEFAULT_PORT, FALLBACK_PORT] {
        if let Ok(l) = StdTcpListener::bind(("127.0.0.1", port)) {
            return Ok((l, port));
        }
    }
    Err(anyhow!(
        "ports {DEFAULT_PORT} and {FALLBACK_PORT} are both in use — Codex's own login is \
         probably already running in a browser or terminal. Finish or close it, then try again."
    ))
}

fn authorize_url(redirect_uri: &str, challenge: &str, state: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(&format!("{ISSUER}/oauth/authorize"))
        .context("parsing the authorize URL")?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", SCOPES)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("originator", ORIGINATOR)
        .append_pair("state", state);
    Ok(url.to_string())
}

/// Wait for the one redirect we care about and answer with a page the user can
/// close. Browsers also hit a loopback server with favicon and prefetch
/// requests, so anything that isn't the callback path is answered and ignored
/// rather than treated as a failed login.
async fn wait_for_callback(listener: TcpListener) -> Result<(String, String)> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(CALLBACK_TIMEOUT_SECS);
    loop {
        let (mut stream, _) = tokio::time::timeout_at(deadline, listener.accept())
            .await
            .context("timed out waiting for the browser login to finish")?
            .context("accepting the OAuth callback connection")?;

        let mut buf = [0u8; 8192];
        let n = stream
            .read(&mut buf)
            .await
            .context("reading the OAuth callback request")?;
        let request = String::from_utf8_lossy(&buf[..n]);
        let target = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("/")
            .to_string();

        let base = reqwest::Url::parse("http://localhost").unwrap();
        let full = base.join(&target).context("parsing the callback path")?;
        if full.path() != CALLBACK_PATH {
            respond(&mut stream, "Not found", "This is Switch NextUp's login listener.").await;
            continue;
        }

        let mut code = None;
        let mut state = None;
        let mut error = None;
        let mut error_description = None;
        for (k, v) in full.query_pairs() {
            match k.as_ref() {
                "code" => code = Some(v.into_owned()),
                "state" => state = Some(v.into_owned()),
                "error" => error = Some(v.into_owned()),
                "error_description" => error_description = Some(v.into_owned()),
                _ => {}
            }
        }

        if let Some(err) = error {
            let detail = error_description.unwrap_or_default();
            respond(&mut stream, "Login failed", "You can close this tab and return to Switch NextUp.").await;
            return Err(anyhow!("login was not completed: {err} {detail}").context("authorization denied"));
        }
        let (Some(code), Some(state)) = (code, state) else {
            respond(&mut stream, "Login failed", "The callback was missing its code or state.").await;
            return Err(anyhow!("the OAuth callback carried no code/state"));
        };
        respond(
            &mut stream,
            "Signed in",
            "You can close this tab and return to Switch NextUp.",
        )
        .await;
        return Ok((code, state));
    }
}

async fn respond(stream: &mut tokio::net::TcpStream, title: &str, body: &str) {
    let html = format!(
        "<html><head><title>{title}</title></head>\
         <body style=\"font-family:sans-serif;padding:2em\"><h3>{title}</h3><p>{body}</p></body></html>"
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        html.len(),
        html
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// Exchange the code. Form-encoded, matching the real client — this endpoint is
/// not the JSON one Anthropic's is.
async fn exchange_code(code: &str, verifier: &str, redirect_uri: &str) -> Result<Value> {
    let resp = reqwest::Client::new()
        .post(format!("{ISSUER}/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", CLIENT_ID),
            ("code_verifier", verifier),
        ])
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .context("token exchange request failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("token exchange failed ({status}): {body}"));
    }
    resp.json::<Value>()
        .await
        .context("parsing the token exchange response")
}

/// Assemble the `auth.json` blob Codex itself writes, so the file we later
/// install as the live credential is byte-shaped like the real one.
fn build_blob(tokens: &Value) -> Result<Value> {
    let get = |k: &str| tokens.get(k).and_then(|v| v.as_str()).map(String::from);
    let id_token = get("id_token").ok_or_else(|| anyhow!("token response has no id_token"))?;
    let access_token =
        get("access_token").ok_or_else(|| anyhow!("token response has no access_token"))?;
    let refresh_token =
        get("refresh_token").ok_or_else(|| anyhow!("token response has no refresh_token"))?;

    let mut blob = json!({
        "OPENAI_API_KEY": Value::Null,
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": id_token,
            "access_token": access_token,
            "refresh_token": refresh_token,
            "account_id": Value::Null,
        },
        "last_refresh": chrono::Utc::now().to_rfc3339(),
    });

    // `tokens.account_id` is the same value as the id_token's
    // `chatgpt_account_id` claim; provider.rs already decodes that, so derive it
    // rather than parsing the JWT a second time here.
    if let Some(account_id) = provider::identity_from_blob(Provider::Chatgpt, &blob)
        .and_then(|id| id.get("accountId").and_then(|v| v.as_str()).map(String::from))
    {
        blob["tokens"]["account_id"] = json!(account_id);
    }
    Ok(blob)
}

/// Run the whole flow and return `(blob, identity)` shaped the way `store`
/// expects. Touches no file — the caller decides where it gets written.
pub async fn login() -> Result<(Value, Value)> {
    let (std_listener, port) = bind_loopback()?;
    std_listener
        .set_nonblocking(true)
        .context("setting the loopback listener non-blocking")?;
    let listener =
        TcpListener::from_std(std_listener).context("adopting the loopback listener into tokio")?;

    let verifier = random_token(32);
    let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
    let state = random_token(32);
    let redirect_uri = format!("http://localhost:{port}{CALLBACK_PATH}");
    let url = authorize_url(&redirect_uri, &challenge, &state)?;

    tauri_plugin_opener::open_url(&url, None::<&str>)
        .map_err(|e| anyhow!("could not open the system browser: {e}"))?;

    let (code, returned_state) = wait_for_callback(listener).await?;
    if returned_state != state {
        return Err(anyhow!(
            "the login response did not match this attempt — aborting for safety"
        ));
    }

    let tokens = exchange_code(&code, &verifier, &redirect_uri).await?;
    let blob = build_blob(&tokens)?;
    let identity = provider::identity_from_blob(Provider::Chatgpt, &blob)
        .ok_or_else(|| anyhow!("signed in, but the id_token carried no account identity"))?;
    Ok((blob, identity))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The redirect_uri is registered WITH its port, so the authorize request
    /// has to carry the real client's exact parameter set — this is the check
    /// the Claude flow lacked until a login failed in the user's hands.
    #[test]
    fn authorize_url_matches_the_real_clients_parameters() {
        let url = authorize_url("http://localhost:1455/auth/callback", "chal", "st8").unwrap();
        let parsed = reqwest::Url::parse(&url).unwrap();
        let q: std::collections::HashMap<String, String> = parsed
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();

        assert_eq!(parsed.host_str(), Some("auth.openai.com"));
        assert_eq!(parsed.path(), "/oauth/authorize");
        assert_eq!(q.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(q.get("client_id").map(String::as_str), Some(CLIENT_ID));
        assert_eq!(
            q.get("redirect_uri").map(String::as_str),
            Some("http://localhost:1455/auth/callback"),
        );
        assert_eq!(
            q.get("scope").map(String::as_str),
            Some("openid profile email offline_access api.connectors.read api.connectors.invoke"),
        );
        assert_eq!(q.get("code_challenge_method").map(String::as_str), Some("S256"));
        assert_eq!(q.get("code_challenge").map(String::as_str), Some("chal"));
        assert_eq!(q.get("state").map(String::as_str), Some("st8"));
        assert_eq!(q.get("id_token_add_organizations").map(String::as_str), Some("true"));
        assert_eq!(q.get("codex_cli_simplified_flow").map(String::as_str), Some("true"));
        assert_eq!(q.get("originator").map(String::as_str), Some(ORIGINATOR));
    }

    /// The blob must be the shape Codex itself writes, because a switch installs
    /// it verbatim as ~/.codex/auth.json.
    #[test]
    fn built_blob_has_codexs_own_auth_json_shape() {
        // id_token payload: {"https://api.openai.com/auth":{"chatgpt_account_id":"acct-7"}}
        let payload = "eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdC03In19";
        let id_token = format!("h.{payload}.s");
        let blob = build_blob(&json!({
            "id_token": id_token,
            "access_token": "at",
            "refresh_token": "rt",
        }))
        .unwrap();

        assert_eq!(blob["auth_mode"], "chatgpt");
        assert!(blob["OPENAI_API_KEY"].is_null());
        assert_eq!(blob["tokens"]["access_token"], "at");
        assert_eq!(blob["tokens"]["refresh_token"], "rt");
        // Derived from the id_token claim, matching what Codex records.
        assert_eq!(blob["tokens"]["account_id"], "acct-7");
        assert!(blob["last_refresh"].as_str().is_some_and(|s| s.contains('T')));

        // And the blob must be readable back by the provider layer that every
        // switch and match goes through.
        let id = provider::identity_from_blob(Provider::Chatgpt, &blob).unwrap();
        assert_eq!(
            provider::stable_id(Provider::Chatgpt, &id).as_deref(),
            Some("acct-7")
        );
    }

    #[test]
    fn a_token_response_missing_a_token_is_rejected() {
        assert!(build_blob(&json!({ "access_token": "at", "refresh_token": "rt" })).is_err());
        assert!(build_blob(&json!({ "id_token": "h.e30.s", "refresh_token": "rt" })).is_err());
        assert!(build_blob(&json!({ "id_token": "h.e30.s", "access_token": "at" })).is_err());
    }
}
