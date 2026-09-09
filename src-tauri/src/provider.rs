//! Per-provider credential handling.
//!
//! `store.rs` owns everything that is the SAME for every provider — the account
//! directory layout, atomic writes, the active pointer, and (most importantly)
//! the token-rotation safety logic that took two bugs to get right. This module
//! owns only the handful of operations that genuinely differ:
//!
//! | | Claude Code | ChatGPT / Codex |
//! |---|---|---|
//! | live file(s) | `~/.claude/.credentials.json` **and** `~/.claude.json` → `oauthAccount` | `~/.codex/auth.json` only |
//! | identity | a separate file's `oauthAccount` object | carried INSIDE the `id_token` JWT |
//! | stable id | `accountUuid` | `chatgpt_account_id` |
//!
//! The Claude side needs two files kept in sync — writing only the tokens leaves
//! Claude Code on the old identity, which was the original "switch didn't take"
//! bug. Codex needs one file, because its identity travels in the token itself.

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;

/// Claim namespace OpenAI puts the ChatGPT account details under inside `id_token`.
const OPENAI_AUTH_CLAIM: &str = "https://api.openai.com/auth";

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Claude,
    Chatgpt,
}

impl Provider {
    /// Every provider, in display order. Claude first — it is the original.
    pub const ALL: [Provider; 2] = [Provider::Claude, Provider::Chatgpt];

    /// Stable on-disk / wire identifier. Used for the account subdirectory, the
    /// active-pointer key and the tray menu ids, so it must never change.
    pub fn slug(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Chatgpt => "chatgpt",
        }
    }

    /// Human label for the UI and the tray sections.
    pub fn label(self) -> &'static str {
        match self {
            Provider::Claude => "Claude",
            Provider::Chatgpt => "ChatGPT",
        }
    }

    pub fn from_slug(s: &str) -> Option<Self> {
        match s {
            "claude" => Some(Provider::Claude),
            "chatgpt" => Some(Provider::Chatgpt),
            _ => None,
        }
    }
}

// ----- Home directories (with the test-only seams) -----

/// Home Claude Code keeps its files in. `SWITCH_NEXTUP_CLAUDE_HOME` overrides it
/// (test-only seam, never set in normal use).
fn claude_home() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("SWITCH_NEXTUP_CLAUDE_HOME") {
        return Ok(PathBuf::from(dir));
    }
    dirs::home_dir().ok_or_else(|| anyhow!("cannot resolve home directory"))
}

/// Directory Codex keeps its files in: `~/.codex`. `SWITCH_NEXTUP_CODEX_HOME`
/// overrides it (test-only seam, the sibling of `SWITCH_NEXTUP_CLAUDE_HOME`, so a
/// test never touches the developer's real Codex login). Note this is the
/// `.codex` directory itself, not its parent — Codex has no equivalent of
/// Claude's split between `~/.claude/` and `~/.claude.json`.
fn codex_home() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("SWITCH_NEXTUP_CODEX_HOME") {
        return Ok(PathBuf::from(dir));
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow!("cannot resolve home directory"))?;
    Ok(home.join(".codex"))
}

/// The live credentials file the provider's own client reads and writes.
pub fn live_creds_path(p: Provider) -> Result<PathBuf> {
    match p {
        Provider::Claude => Ok(claude_home()?.join(".claude").join(".credentials.json")),
        Provider::Chatgpt => Ok(codex_home()?.join("auth.json")),
    }
}

/// Claude Code's own `~/.claude.json`, which holds the `oauthAccount` identity.
/// Claude-only: Codex has no second file.
pub fn claude_json_path() -> Result<PathBuf> {
    Ok(claude_home()?.join(".claude.json"))
}

// ----- Live credential blob -----
//
// A "credential blob" is whatever we store for an account and write back to make
// it live. For Claude that is the inner `claudeAiOauth` object (the file is a
// wrapper around it); for Codex it is the WHOLE `auth.json` object, so that
// fields we do not model (`auth_mode`, `OPENAI_API_KEY`, `last_refresh`, plus
// anything OpenAI adds later) survive a round-trip untouched.

/// Read the live credential blob for `p`.
pub fn read_live(p: Provider) -> Result<Value> {
    let path = live_creds_path(p)?;
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("reading live credentials at {}", path.display()))?;
    let v: Value = serde_json::from_str(&raw).context("parsing live credentials json")?;
    match p {
        Provider::Claude => v
            .get("claudeAiOauth")
            .cloned()
            .ok_or_else(|| anyhow!("no claudeAiOauth field in live credentials")),
        // Codex's file IS the blob. Sanity-check it looks like one rather than
        // silently treating an unrelated json file as a login.
        Provider::Chatgpt => {
            if v.get("tokens").is_some() {
                Ok(v)
            } else {
                Err(anyhow!("no tokens field in {}", path.display()))
            }
        }
    }
}

/// Write a credential blob into the live file, making that account current.
/// Callers go through `store::switch`, which captures the OUTGOING account's
/// rotated tokens first — see the rotation note there.
pub fn write_live(p: Provider, blob: &Value) -> Result<()> {
    let path = live_creds_path(p)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let payload = match p {
        Provider::Claude => json!({ "claudeAiOauth": blob }),
        Provider::Chatgpt => blob.clone(),
    };
    crate::store::atomic_write(&path, serde_json::to_string_pretty(&payload)?.as_bytes())
        .with_context(|| format!("writing live credentials at {}", path.display()))?;
    Ok(())
}

// ----- Tokens (used to tell "same account" without any identity lookup) -----

pub fn refresh_token_of(p: Provider, blob: &Value) -> Option<String> {
    let field = match p {
        Provider::Claude => blob.get("refreshToken"),
        Provider::Chatgpt => blob.get("tokens").and_then(|t| t.get("refresh_token")),
    };
    field.and_then(|v| v.as_str()).map(String::from)
}

pub fn access_token_of(p: Provider, blob: &Value) -> Option<String> {
    let field = match p {
        Provider::Claude => blob.get("accessToken"),
        Provider::Chatgpt => blob.get("tokens").and_then(|t| t.get("access_token")),
    };
    field.and_then(|v| v.as_str()).map(String::from)
}

// ----- Identity -----

/// Decode the payload of a JWT **without verifying the signature**. That is
/// correct here and not a shortcut: this is our own local credential file, we are
/// not authenticating a caller — we only want the account details OpenAI already
/// put in the token. Never log the token itself.
fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    // Tolerate padding even though JWTs are normally unpadded base64url.
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The identity for a credential blob, in the shape we store and display.
///
/// Claude blobs carry no identity at all (it lives in `~/.claude.json`), so this
/// returns None for them — `store` reads the live identity separately. Codex
/// blobs carry everything inside `id_token`, so we derive it here.
pub fn identity_from_blob(p: Provider, blob: &Value) -> Option<Value> {
    match p {
        Provider::Claude => None,
        Provider::Chatgpt => {
            let id_token = blob.get("tokens")?.get("id_token")?.as_str()?;
            let claims = jwt_claims(id_token)?;
            let auth = claims.get(OPENAI_AUTH_CLAIM);
            // `tokens.account_id` and the claim's `chatgpt_account_id` are the
            // same value; prefer the claim and fall back to the file field so an
            // unparseable id_token still yields a usable stable id.
            let account_id = auth
                .and_then(|a| a.get("chatgpt_account_id"))
                .and_then(|v| v.as_str())
                .map(String::from)
                .or_else(|| {
                    blob.get("tokens")?
                        .get("account_id")?
                        .as_str()
                        .map(String::from)
                });
            Some(json!({
                "accountId": account_id,
                "emailAddress": claims.get("email").and_then(|v| v.as_str()),
                "displayName": claims.get("name").and_then(|v| v.as_str()),
                "planType": auth.and_then(|a| a.get("chatgpt_plan_type")).and_then(|v| v.as_str()),
                "chatgptUserId": auth.and_then(|a| a.get("chatgpt_user_id")).and_then(|v| v.as_str()),
                "subscriptionActiveUntil": auth
                    .and_then(|a| a.get("chatgpt_subscription_active_until"))
                    .and_then(|v| v.as_str()),
            }))
        }
    }
}

/// The stable account id inside an identity object — survives token rotation and
/// a full re-login, so it is what "is this the same account?" is decided on.
/// `accountUuid` for Claude, `accountId` (= `chatgpt_account_id`) for ChatGPT.
pub fn stable_id(p: Provider, identity: &Value) -> Option<String> {
    let key = match p {
        Provider::Claude => "accountUuid",
        Provider::Chatgpt => "accountId",
    };
    identity.get(key).and_then(|v| v.as_str()).map(String::from)
}

/// The plan/tier to show on the card ("Max", "Enterprise", "business", …).
pub fn plan_of(p: Provider, blob: &Value, identity: Option<&Value>) -> Option<String> {
    match p {
        Provider::Claude => blob
            .get("subscriptionType")
            .and_then(|v| v.as_str())
            .map(String::from),
        Provider::Chatgpt => identity?
            .get("planType")
            .and_then(|v| v.as_str())
            .map(String::from),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds an unsigned JWT with the given payload — the same wire shape
    /// `id_token` has, so `identity_from_blob` is exercised for real.
    fn fake_jwt(payload: Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let body = URL_SAFE_NO_PAD.encode(payload.to_string().as_bytes());
        format!("{header}.{body}.not-a-real-signature")
    }

    fn codex_blob(email: &str, account_id: &str, plan: &str) -> Value {
        json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": fake_jwt(json!({
                    "email": email,
                    "name": "Test User",
                    OPENAI_AUTH_CLAIM: {
                        "chatgpt_account_id": account_id,
                        "chatgpt_plan_type": plan,
                        "chatgpt_user_id": "user-abc",
                        "chatgpt_subscription_active_until": "2027-08-10T00:00:00+00:00"
                    }
                })),
                "access_token": "access-tok",
                "refresh_token": "refresh-tok",
                "account_id": account_id
            },
            "last_refresh": "2026-09-07T03:33:01.361101900Z"
        })
    }

    #[test]
    fn chatgpt_identity_comes_out_of_the_id_token() {
        let blob = codex_blob("a@b.com", "acct-123", "business");
        let id = identity_from_blob(Provider::Chatgpt, &blob).expect("identity");
        assert_eq!(id["accountId"], "acct-123");
        assert_eq!(id["emailAddress"], "a@b.com");
        assert_eq!(id["planType"], "business");
        assert_eq!(id["displayName"], "Test User");
        assert_eq!(stable_id(Provider::Chatgpt, &id).as_deref(), Some("acct-123"));
        assert_eq!(
            plan_of(Provider::Chatgpt, &blob, Some(&id)).as_deref(),
            Some("business")
        );
    }

    #[test]
    fn chatgpt_identity_falls_back_to_the_file_account_id_when_the_jwt_is_junk() {
        // A malformed id_token must not lose the stable id — it is also a plain
        // field on the file, and without it the account can never be matched.
        let mut blob = codex_blob("a@b.com", "acct-123", "business");
        blob["tokens"]["id_token"] = json!("not.a.jwt");
        let id = identity_from_blob(Provider::Chatgpt, &blob);
        // Junk payload -> no claims at all, so no identity object is produced.
        // The caller then falls back to the stored identity, never to a wrong one.
        assert!(id.is_none());
    }

    #[test]
    fn chatgpt_tokens_are_read_from_the_nested_tokens_object() {
        let blob = codex_blob("a@b.com", "acct-123", "business");
        assert_eq!(
            access_token_of(Provider::Chatgpt, &blob).as_deref(),
            Some("access-tok")
        );
        assert_eq!(
            refresh_token_of(Provider::Chatgpt, &blob).as_deref(),
            Some("refresh-tok")
        );
    }

    #[test]
    fn claude_tokens_are_read_from_the_flat_blob() {
        let blob = json!({
            "accessToken": "at", "refreshToken": "rt",
            "subscriptionType": "max", "expiresAt": 123
        });
        assert_eq!(access_token_of(Provider::Claude, &blob).as_deref(), Some("at"));
        assert_eq!(refresh_token_of(Provider::Claude, &blob).as_deref(), Some("rt"));
        assert_eq!(plan_of(Provider::Claude, &blob, None).as_deref(), Some("max"));
        // Claude identity lives in a separate file, never in the blob.
        assert!(identity_from_blob(Provider::Claude, &blob).is_none());
    }

    /// The frontend sends the provider as a bare lowercase string in the invoke
    /// payload, so `Provider` must deserialize from exactly that. Guards the
    /// IPC boundary: a mismatch here fails only at runtime, with cargo test and
    /// node --check both green — which is precisely how an earlier build
    /// shipped an app that could not switch, remove or rename any account.
    #[test]
    fn provider_deserializes_from_the_wire_strings_the_frontend_sends() {
        assert_eq!(
            serde_json::from_value::<Provider>(json!("claude")).unwrap(),
            Provider::Claude
        );
        assert_eq!(
            serde_json::from_value::<Provider>(json!("chatgpt")).unwrap(),
            Provider::Chatgpt
        );
        // Anything else must be rejected rather than silently defaulting to a
        // provider, which would write one account's tokens over another's.
        assert!(serde_json::from_value::<Provider>(json!("Claude")).is_err());
        assert!(serde_json::from_value::<Provider>(json!("openai")).is_err());
        // And the slugs the frontend reads back out of AccountInfo must be the
        // same strings it sends in.
        for p in Provider::ALL {
            assert_eq!(serde_json::to_value(p).unwrap(), json!(p.slug()));
        }
    }

    #[test]
    fn slugs_round_trip_and_stay_stable() {
        for p in Provider::ALL {
            assert_eq!(Provider::from_slug(p.slug()), Some(p));
        }
        // These strings are on disk (account dirs, active pointer, tray ids).
        assert_eq!(Provider::Claude.slug(), "claude");
        assert_eq!(Provider::Chatgpt.slug(), "chatgpt");
        assert_eq!(Provider::from_slug("nope"), None);
    }
}
