use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;

use crate::provider::{self, Provider};
use crate::store;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const OAUTH_BETA: &str = "oauth-2025-04-20";
// Refresh when the token expires within this window (ms).
const REFRESH_SKEW_MS: i64 = 5 * 60 * 1000;

// ChatGPT/Codex usage — see docs/usage-api.md -> "Usage endpoint" for how this
// was found and the exact captured response shape.
const CHATGPT_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const CHATGPT_USER_AGENT: &str =
    "codex_vscode/0.4.71 (Windows NT 10.0; x86_64) unknown (VS Code; 0.4.71)";

/// Returned by `refresh_oauth` when the server rejects the refresh token (4xx).
/// Distinct from a transient network/server error so callers can surface a
/// "session expired — please log in again" message without retrying.
#[derive(Debug)]
struct SessionExpiredError;
impl std::fmt::Display for SessionExpiredError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "session expired — run `claude /login` and re-save this account")
    }
}
impl std::error::Error for SessionExpiredError {}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Bucket {
    /// Raw API key (codename), e.g. "five_hour" or "cinder_cove".
    pub key: String,
    /// Friendly display label (mirrors Claude Code's /usage).
    pub label: String,
    /// Optional extra descriptor, e.g. "One-time credit".
    pub subtext: Option<String>,
    /// Percent used, 0-100 (already scaled by the API).
    pub utilization: f64,
    pub resets_at: Option<String>,
    /// Dollar figures, present only for credit-style buckets.
    pub limit_dollars: Option<f64>,
    pub used_dollars: Option<f64>,
    pub remaining_dollars: Option<f64>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Usage {
    pub buckets: Vec<Bucket>,
    pub extra_usage_enabled: bool,
    pub extra_usage_utilization: Option<f64>,
    /// Unix ms when this data was actually fetched from the API.
    #[serde(default)]
    pub fetched_at: Option<i64>,
    /// True when this is last-known data served because a live fetch failed.
    #[serde(default)]
    pub stale: bool,
    /// Why it's stale (e.g. the fetch error), for display.
    #[serde(default)]
    pub note: Option<String>,
    /// ChatGPT's `credits` object doesn't fit the utilization-bucket shape (no
    /// percentage, just a balance/unlimited flag), so it renders as a plain
    /// note instead of a fabricated meter. `#[serde(default)]` so cached files
    /// written before this field existed still deserialize. Always None on the
    /// Claude side.
    #[serde(default)]
    pub credits_note: Option<String>,
}

// ---- last-known usage persistence (so an account still shows numbers when the
// rate-limit-happy endpoint 429s, or we're offline) ----

/// Cache path for an account's last-known usage. Delegates to
/// `store::usage_file` rather than recomputing the layout, so remove/rename/clear
/// can never disagree with this about where the file lives (they used to compute
/// it independently).
fn usage_cache_file(p: Provider, name: &str) -> Option<PathBuf> {
    let path = store::usage_file(p, name).ok()?;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    Some(path)
}

fn read_cached_usage(p: Provider, name: &str) -> Option<Usage> {
    let raw = match usage_cache_file(p, name).and_then(|path| std::fs::read_to_string(path).ok()) {
        Some(raw) => raw,
        // Pre-multi-provider builds cached at `usage/<name>.json` with no
        // provider subdirectory — Claude-only, since ChatGPT support (and its
        // cache) was added after the provider split. Read it rather than
        // blanking every card's "As of …" numbers on upgrade.
        None if p == Provider::Claude => {
            let legacy = store::legacy_usage_file(name).ok()?;
            std::fs::read_to_string(legacy).ok()?
        }
        None => return None,
    };
    serde_json::from_str(&raw).ok()
}

fn write_cached_usage(p: Provider, name: &str, usage: &Usage) {
    if let Some(path) = usage_cache_file(p, name) {
        if let Ok(s) = serde_json::to_string_pretty(usage) {
            let _ = std::fs::write(path, s);
        }
    }
}

/// Last-known persisted usage for an account, WITHOUT any network call. Used for
/// non-active accounts so they show their most recent numbers (from whenever they
/// were last the active/fetched account) without hitting the rate-limited usage
/// endpoint. Returns None if no successful fetch was ever persisted. The `note` is
/// cleared because this is not a failed live fetch — the UI renders it quietly.
pub fn cached_usage_for(p: Provider, name: &str) -> Option<Usage> {
    let mut u = read_cached_usage(p, name)?;
    u.note = None;
    Some(u)
}

// Known buckets in display order, with friendly labels mirroring Claude Code's
// /usage. The response for credit-based plans (Enterprise, etc.) uses *volatile
// internal codename* buckets with `_dollars` fields (e.g. `cinder_cove` = the
// "Claude Code and Cowork credit"); these codenames churn between releases
// (seen: tangelo, iguana_necktie, nimbus_quill, amber_ladder, …). We render the
// known ones nicely and fall back to surfacing any other used bucket generically
// so real usage never silently vanishes when a codename is renamed.
const KNOWN_BUCKETS: &[(&str, &str, Option<&str>)] = &[
    ("five_hour", "Current session", None),
    ("seven_day", "Current week (all models)", None),
    ("seven_day_sonnet", "Current week (Sonnet only)", None),
    ("seven_day_opus", "Current week (Opus only)", None),
    // Not a utilization bucket — parsed by `parse_spend`. Listed here so it lands
    // above the credit bucket, matching Claude Code's own panel (plan limit first,
    // one-time credit after).
    (SPEND_KEY, "Spend limit", None),
    ("cinder_cove", "Claude Code and Cowork credit", Some("One-time credit")),
];

/// The `spend` object: the plan's real money limit, shaped unlike every other
/// bucket (see `parse_spend`).
const SPEND_KEY: &str = "spend";

// Top-level fields that are NOT utilization meters — skip in the generic sweep.
// (`spend` is one too, but it IS rendered, via KNOWN_BUCKETS + `parse_spend`.)
const NON_BUCKET_KEYS: &[&str] = &["extra_usage", "limits", "member_dashboard_available"];

fn parse_bucket(key: &str, label: &str, subtext: Option<&str>, v: &Value) -> Option<Bucket> {
    let obj = v.as_object()?;
    let util = obj.get("utilization")?;
    if util.is_null() {
        return None;
    }
    let dollar = |k: &str| obj.get(k).and_then(|x| x.as_f64());
    Some(Bucket {
        key: key.to_string(),
        label: label.to_string(),
        subtext: subtext.map(String::from),
        utilization: util.as_f64().unwrap_or(0.0),
        resets_at: obj.get("resets_at").and_then(|x| x.as_str()).map(String::from),
        limit_dollars: dollar("limit_dollars"),
        used_dollars: dollar("used_dollars"),
        remaining_dollars: dollar("remaining_dollars"),
    })
}

/// Parse the `spend` object — the plan's **real** money limit, which Claude Code's
/// usage panel shows as "$0.00 of $150.00 spent · Spend limit · 0% used" above the
/// one-time credit. It is shaped unlike the utilization buckets: `percent` instead
/// of `utilization`, and money as **minor units + exponent**
/// (`{ amount_minor: 15000, currency: "USD", exponent: 2 }` = $150.00).
///
/// Skipping it (it used to sit in `NON_BUCKET_KEYS`) is what hid an Enterprise
/// account's actual limit and left only the promotional credit bucket on screen —
/// `extra_usage.utilization` is null on such a plan, so that line renders nothing
/// either. Verified live 2026-09-07; see `docs/usage-api.md` -> "Spend limit".
///
/// Returns None when the plan has no real spend limit (personal Pro/Max come back
/// `enabled: false` with no `limit`), so those grow no empty row.
fn parse_spend(label: &str, v: &Value) -> Option<Bucket> {
    let obj = v.as_object()?;
    if !obj.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false) {
        return None;
    }
    // Minor units -> major (15000 with exponent 2 -> 150.00).
    let money = |k: &str| -> Option<f64> {
        let m = obj.get(k)?.as_object()?;
        let minor = m.get("amount_minor")?.as_f64()?;
        let exp = m.get("exponent").and_then(|x| x.as_i64()).unwrap_or(2) as i32;
        Some(minor / 10f64.powi(exp))
    };
    let limit = money("limit")?;
    let used = money("used").unwrap_or(0.0);
    Some(Bucket {
        key: SPEND_KEY.to_string(),
        label: label.to_string(),
        subtext: None,
        utilization: obj.get("percent").and_then(|x| x.as_f64()).unwrap_or(0.0),
        // The response carries NO reset timestamp for the spend window — the
        // official panel derives one ("Resets Oct 1" = the next UTC month), which
        // assumes a calendar-month cycle we cannot confirm. Show none rather than
        // print a date that might be wrong.
        resets_at: None,
        limit_dollars: Some(limit),
        used_dollars: Some(used),
        remaining_dollars: Some((limit - used).max(0.0)),
    })
}

/// Title-case an unknown codename ("amber_ladder" -> "Amber Ladder").
fn humanize(key: &str) -> String {
    key.split('_')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_usage(v: &Value) -> Usage {
    let mut buckets = Vec::new();

    // Known buckets first, in display order.
    for (key, label, subtext) in KNOWN_BUCKETS {
        if let Some(val) = v.get(*key) {
            let parsed = if *key == SPEND_KEY {
                parse_spend(label, val)
            } else {
                parse_bucket(key, label, *subtext, val)
            };
            if let Some(b) = parsed {
                buckets.push(b);
            }
        }
    }

    // Safety net for renamed/new codename buckets: surface any other object with
    // a non-null, non-zero utilization. Empty org pools (0%) stay hidden to match
    // Claude Code's /usage output.
    if let Some(obj) = v.as_object() {
        for (key, val) in obj {
            if KNOWN_BUCKETS.iter().any(|(k, _, _)| k == key)
                || NON_BUCKET_KEYS.contains(&key.as_str())
            {
                continue;
            }
            let label = humanize(key);
            if let Some(b) = parse_bucket(key, &label, None, val) {
                if b.utilization > 0.0 {
                    buckets.push(b);
                }
            }
        }
    }

    let extra = v.get("extra_usage");
    Usage {
        buckets,
        extra_usage_enabled: extra
            .and_then(|e| e.get("is_enabled"))
            .and_then(|b| b.as_bool())
            .unwrap_or(false),
        extra_usage_utilization: extra
            .and_then(|e| e.get("utilization"))
            .and_then(|u| u.as_f64()),
        ..Default::default()
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Refresh an expiring oauth token. Returns the updated oauth object (with new
/// accessToken / refreshToken / expiresAt) on success.
async fn refresh_oauth(oauth: &Value) -> Result<Value> {
    let refresh_token = oauth
        .get("refreshToken")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("account has no refresh token"))?;
    // `scopes` is stored the way Claude Code writes it: a JSON ARRAY
    // (e.g. ["user:inference", "user:profile", ...]). The token endpoint wants a
    // single space-joined string, so join it. Reading it as `.as_str()` (the old
    // bug) returned None for an array -> an empty scope was sent on every refresh,
    // which broke token refresh for any account whose token had expired. That hit
    // non-active accounts specifically: the active account's token is kept fresh by
    // Claude Code (so it rarely needs our refresh), while an idle non-active
    // account's token is always stale and refreshed here every fetch. A plain
    // string is tolerated too, for robustness.
    let scope = match oauth.get("scopes") {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join(" "),
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    };

    let client = reqwest::Client::new();
    let resp = client
        .post(TOKEN_URL)
        .header("Content-Type", "application/json")
        .json(&json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": CLIENT_ID,
            "scope": scope,
        }))
        .send()
        .await?;

    if !resp.status().is_success() {
        let code = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if code.is_client_error() {
            // 4xx means the refresh token itself is invalid/expired — not transient.
            return Err(anyhow::Error::new(SessionExpiredError));
        }
        return Err(anyhow!("token refresh failed ({code}): {body}"));
    }

    let body: Value = resp.json().await?;
    let mut updated = oauth.clone();
    if let Some(obj) = updated.as_object_mut() {
        if let Some(at) = body.get("access_token").and_then(|v| v.as_str()) {
            obj.insert("accessToken".into(), json!(at));
        }
        if let Some(rt) = body.get("refresh_token").and_then(|v| v.as_str()) {
            obj.insert("refreshToken".into(), json!(rt));
        }
        if let Some(exp) = body.get("expires_in").and_then(|v| v.as_i64()) {
            obj.insert("expiresAt".into(), json!(now_ms() + exp * 1000));
        }
    }
    Ok(updated)
}

/// Returns stale cached usage (or an empty stale Usage) annotated with a
/// session-expired note. Used to surface a clear "log in again" message.
fn session_expired_result(p: Provider, name: &str) -> Result<Usage> {
    let msg = "session expired — run `claude /login` and re-save this account".to_string();
    let mut base = read_cached_usage(p, name).unwrap_or_default();
    base.stale = true;
    base.note = Some(msg);
    Ok(base)
}

/// Fetch usage for a single stored account, live-fetching only ever the
/// provider's own API. Only the Claude path refreshes tokens itself. ChatGPT
/// *does* have a refresh grant (auth.openai.com/oauth/token — see
/// docs/usage-api.md), but nothing here needs it: `resolve_account_oauth`
/// already keeps a ChatGPT account's stored copy in sync with whatever Codex
/// itself last wrote to the live file.
pub async fn fetch_usage_for(p: Provider, name: &str) -> Result<Usage> {
    match p {
        Provider::Claude => fetch_claude_usage_for(name).await,
        Provider::Chatgpt => fetch_chatgpt_usage_for(name).await,
    }
}

/// Fetch usage for a single stored Claude account. Takes the tokens Claude Code
/// is actually using when this account is the live one (`resolve_account_oauth`
/// — the stored copy drifts as CC rotates tokens, and refreshing with a
/// rotated-away token looks like an expired session), refreshes them first if
/// they are expired/expiring, and persists a refreshed token back to the account
/// store (and to the live credentials file when that account is active).
async fn fetch_claude_usage_for(name: &str) -> Result<Usage> {
    let mut oauth = store::resolve_account_oauth(Provider::Claude, name)?;

    let expires_at = oauth.get("expiresAt").and_then(|v| v.as_i64()).unwrap_or(0);
    let needs_refresh = expires_at != 0 && now_ms() + REFRESH_SKEW_MS >= expires_at;

    if needs_refresh {
        match refresh_oauth(&oauth).await {
            Ok(updated) => {
                let was_active = is_active_account(name, &oauth);
                oauth = updated;
                let _ = store::write_account_oauth(Provider::Claude, name, &oauth);
                if was_active {
                    let _ = store::write_live_oauth(Provider::Claude, &oauth);
                }
            }
            Err(e) if e.is::<SessionExpiredError>() => {
                return session_expired_result(Provider::Claude, name);
            }
            Err(e) => {
                // Transient failure — fall through and try the existing token.
                eprintln!("refresh for '{name}' failed, using existing token: {e}");
            }
        }
    }

    let access_token = oauth
        .get("accessToken")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("account has no access token"))?
        .to_string();

    let mut fetched = request_usage(&access_token).await;

    // On 401 try a one-time forced refresh, then retry once.
    if let Err(FetchError::Unauthorized) = &fetched {
        match refresh_oauth(&oauth).await {
            Ok(updated) => {
                let was_active = is_active_account(name, &oauth);
                let _ = store::write_account_oauth(Provider::Claude, name, &updated);
                if was_active {
                    let _ = store::write_live_oauth(Provider::Claude, &updated);
                }
                if let Some(at) = updated.get("accessToken").and_then(|v| v.as_str()) {
                    fetched = request_usage(at).await;
                }
            }
            Err(e) if e.is::<SessionExpiredError>() => {
                return session_expired_result(Provider::Claude, name);
            }
            Err(_) => {}
        }
    }

    match fetched {
        Ok(mut usage) => {
            usage.fetched_at = Some(now_ms());
            usage.stale = false;
            usage.note = None;
            write_cached_usage(Provider::Claude, name, &usage);
            Ok(usage)
        }
        Err(e) => {
            // Fall back to last-known persisted usage so the account still shows
            // numbers (the endpoint 429s easily; also covers offline). The
            // frontend renders it flagged as stale + retries.
            if let Some(mut cached) = read_cached_usage(Provider::Claude, name) {
                cached.stale = true;
                cached.note = Some(e.to_string());
                Ok(cached)
            } else {
                Err(anyhow!("{e}"))
            }
        }
    }
}

/// Fetch usage for a single stored ChatGPT/Codex account. No refresh-and-retry
/// loop here (unlike Claude) — there is no known OpenAI refresh endpoint for
/// this app to call, so a 401 just falls through to the cached-usage fallback
/// like any other fetch failure.
async fn fetch_chatgpt_usage_for(name: &str) -> Result<Usage> {
    let oauth = store::resolve_account_oauth(Provider::Chatgpt, name)?;
    let access_token = provider::access_token_of(Provider::Chatgpt, &oauth)
        .ok_or_else(|| anyhow!("account has no access token"))?;
    let account_id = oauth
        .get("tokens")
        .and_then(|t| t.get("account_id"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("account has no account id"))?
        .to_string();

    match request_chatgpt_usage(&access_token, &account_id).await {
        Ok(mut usage) => {
            usage.fetched_at = Some(now_ms());
            usage.stale = false;
            usage.note = None;
            write_cached_usage(Provider::Chatgpt, name, &usage);
            Ok(usage)
        }
        Err(e) => {
            if let Some(mut cached) = read_cached_usage(Provider::Chatgpt, name) {
                cached.stale = true;
                cached.note = Some(e.to_string());
                Ok(cached)
            } else {
                Err(anyhow!("{e}"))
            }
        }
    }
}

fn is_active_account(name: &str, oauth: &Value) -> bool {
    if let Ok(live) = store::read_live_oauth(Provider::Claude) {
        let live_refresh = live.get("refreshToken").and_then(|v| v.as_str());
        let acc_refresh = oauth.get("refreshToken").and_then(|v| v.as_str());
        if live_refresh.is_some() && live_refresh == acc_refresh {
            return true;
        }
    }
    // Fall back to a name-based active check via the store listing.
    store::list_accounts_for(Provider::Claude)
        .map(|list| list.iter().any(|a| a.name == name && a.is_active))
        .unwrap_or(false)
}

#[derive(Debug)]
enum FetchError {
    Unauthorized,
    RateLimited,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Unauthorized => write!(f, "unauthorized (token rejected)"),
            // The frontend pattern-matches "rate limited"/"429" to show a friendly
            // message and back off — keep those tokens in this string.
            FetchError::RateLimited => {
                write!(f, "rate limited (429): usage endpoint is temporarily throttled")
            }
            FetchError::Other(s) => write!(f, "{s}"),
        }
    }
}

async fn request_usage(access_token: &str) -> std::result::Result<Usage, FetchError> {
    let client = reqwest::Client::new();
    let resp = client
        .get(USAGE_URL)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Content-Type", "application/json")
        .header("anthropic-beta", OAUTH_BETA)
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await
        .map_err(|e| FetchError::Other(e.to_string()))?;

    let status = resp.status();
    if status.as_u16() == 401 {
        return Err(FetchError::Unauthorized);
    }
    if status.as_u16() == 429 {
        return Err(FetchError::RateLimited);
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(FetchError::Other(format!("usage request failed ({status}): {body}")));
    }
    let body: Value = resp
        .json()
        .await
        .map_err(|e| FetchError::Other(e.to_string()))?;
    Ok(parse_usage(&body))
}

// ----- ChatGPT/Codex usage -----
//
// Same shape of capability as the Claude side (queryable on demand, per
// account, from a stored token), but the response is shaped very differently
// and has THREE independent shapes that must all be parsed from the start —
// see docs/usage-api.md -> "Parse all three shapes". A parser that only
// handles `rate_limit` (like the C++ project this endpoint was found via)
// renders nothing at all on a business/workspace account, where the real
// quota lives in `spend_control` instead — precisely the mistake the Claude
// side made with its own `spend` bucket, one vendor over.

/// The `spend_control.individual_limit` bucket's key — the real quota on
/// business/workspace ChatGPT plans, the analogue of Claude's `spend`.
const SPEND_CONTROL_KEY: &str = "spend_control";

/// ChatGPT money fields arrive as JSON **strings** ("600", "94.373..."), not
/// numbers like the Claude side — parse rather than assume a type.
fn chatgpt_money(v: &Value) -> Option<f64> {
    match v {
        Value::String(s) => s.parse::<f64>().ok(),
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

/// `reset_at` on this side is a unix epoch **second** (Anthropic's `resets_at`
/// is already an ISO string) — convert so the frontend's existing
/// `fmtReset`/`windowRolledOver` logic keeps working unmodified for both
/// providers.
fn chatgpt_epoch_to_iso(v: &Value) -> Option<String> {
    let secs = v.as_i64()?;
    Some(chrono::DateTime::from_timestamp(secs, 0)?.to_rfc3339())
}

fn parse_rate_limit_window(key: &str, label: &str, v: &Value) -> Option<Bucket> {
    let obj = v.as_object()?;
    let pct = obj.get("used_percent")?.as_f64()?;
    Some(Bucket {
        key: key.to_string(),
        label: label.to_string(),
        subtext: None,
        utilization: pct,
        resets_at: obj.get("reset_at").and_then(chatgpt_epoch_to_iso),
        limit_dollars: None,
        used_dollars: None,
        remaining_dollars: None,
    })
}

/// The real quota on business/workspace plans — `spend_control.individual_limit`.
/// Shaped like Claude's `spend` bucket (money + percent) but with string money
/// fields and an epoch-second reset instead of minor-units/exponent and no reset.
fn parse_spend_control(v: &Value) -> Option<Bucket> {
    let individual = v.get("individual_limit")?.as_object()?;
    let limit = chatgpt_money(individual.get("limit")?)?;
    let used = individual.get("used").and_then(chatgpt_money).unwrap_or(0.0);
    let remaining = individual
        .get("remaining")
        .and_then(chatgpt_money)
        .unwrap_or((limit - used).max(0.0));
    let pct = individual
        .get("used_percent")
        .and_then(|x| x.as_f64())
        .unwrap_or_else(|| if limit > 0.0 { used / limit * 100.0 } else { 0.0 });
    Some(Bucket {
        key: SPEND_CONTROL_KEY.to_string(),
        label: "Spend limit".to_string(),
        subtext: None,
        utilization: pct,
        resets_at: individual.get("reset_at").and_then(chatgpt_epoch_to_iso),
        limit_dollars: Some(limit),
        used_dollars: Some(used),
        remaining_dollars: Some(remaining),
    })
}

/// `credits` doesn't fit the utilization-bucket shape (no percentage, no
/// limit — just a balance/unlimited flag), so it renders as a plain note
/// rather than a fabricated meter, and only when there is something
/// meaningful to say (the captured live response had `balance: null`, i.e.
/// nothing to show).
fn parse_credits_note(v: &Value) -> Option<String> {
    let credits = v.get("credits")?.as_object()?;
    if !credits.get("has_credits").and_then(|x| x.as_bool()).unwrap_or(false) {
        return None;
    }
    let overage = credits
        .get("overage_limit_reached")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    let suffix = if overage { " (overage limit reached)" } else { "" };
    if credits.get("unlimited").and_then(|x| x.as_bool()).unwrap_or(false) {
        return Some(format!("Unlimited credits{suffix}"));
    }
    let balance = chatgpt_money(credits.get("balance")?)?;
    Some(format!("${balance:.2} credit balance{suffix}"))
}

fn parse_chatgpt_usage(v: &Value) -> Usage {
    let mut buckets = Vec::new();

    // `rate_limit` is present-but-null on business/workspace accounts (only
    // `spend_control` applies there) — `.get("rate_limit")` returns
    // `Some(&Value::Null)` in that case, and `parse_rate_limit_window`'s
    // `.as_object()?` on a null value correctly yields None.
    if let Some(rl) = v.get("rate_limit") {
        let null = Value::Null;
        if let Some(b) =
            parse_rate_limit_window("rate_limit_primary", "Primary rate limit", rl.get("primary_window").unwrap_or(&null))
        {
            buckets.push(b);
        }
        if let Some(b) = parse_rate_limit_window(
            "rate_limit_secondary",
            "Secondary rate limit",
            rl.get("secondary_window").unwrap_or(&null),
        ) {
            buckets.push(b);
        }
    }
    if let Some(sc) = v.get("spend_control") {
        if let Some(b) = parse_spend_control(sc) {
            buckets.push(b);
        }
    }

    Usage {
        buckets,
        credits_note: parse_credits_note(v),
        ..Default::default()
    }
}

async fn request_chatgpt_usage(
    access_token: &str,
    account_id: &str,
) -> std::result::Result<Usage, FetchError> {
    let client = reqwest::Client::new();
    let resp = client
        .get(CHATGPT_USAGE_URL)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("ChatGPT-Account-Id", account_id)
        .header("originator", "codex_vscode")
        .header("User-Agent", CHATGPT_USER_AGENT)
        .header("accept", "*/*")
        .header("accept-language", "*")
        .header("sec-fetch-mode", "cors")
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await
        .map_err(|e| FetchError::Other(e.to_string()))?;

    let status = resp.status();
    if status.as_u16() == 401 {
        return Err(FetchError::Unauthorized);
    }
    if status.as_u16() == 429 {
        return Err(FetchError::RateLimited);
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(FetchError::Other(format!("usage request failed ({status}): {body}")));
    }
    let body: Value = resp
        .json()
        .await
        .map_err(|e| FetchError::Other(e.to_string()))?;
    Ok(parse_chatgpt_usage(&body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket<'a>(u: &'a Usage, key: &str) -> Option<&'a Bucket> {
        u.buckets.iter().find(|b| b.key == key)
    }

    /// Round-trips a bucket's `resets_at` back to a unix second, so tests can
    /// assert against the epoch value in the fixture rather than a hardcoded
    /// RFC3339 string whose exact formatting (offset style, fractional
    /// seconds) is an implementation detail of `chrono::to_rfc3339`.
    fn resets_at_epoch(b: &Bucket) -> Option<i64> {
        b.resets_at
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.timestamp())
    }

    /// Trimmed from the real Enterprise response captured live on 2026-09-07 —
    /// the exact shape that used to render only the promo credit.
    fn enterprise_response() -> Value {
        json!({
            "five_hour": null,
            "seven_day": null,
            "seven_day_sonnet": null,
            "seven_day_opus": null,
            "nimbus_quill": {
                "utilization": 0.0, "resets_at": null,
                "limit_dollars": null, "used_dollars": null, "remaining_dollars": null
            },
            "cinder_cove": {
                "utilization": 98.3529643,
                "resets_at": "2026-09-28T10:06:41.557093+00:00",
                "limit_dollars": 1000, "used_dollars": 983.529643,
                "remaining_dollars": 16.470357000000035
            },
            "extra_usage": {
                "is_enabled": true, "monthly_limit": 15000, "used_credits": 0.0,
                "utilization": null, "currency": "USD", "decimal_places": 2
            },
            "limits": [],
            "spend": {
                "used":  { "amount_minor": 0,     "currency": "USD", "exponent": 2 },
                "limit": { "amount_minor": 15000, "currency": "USD", "exponent": 2 },
                "percent": 0, "severity": "normal", "enabled": true
            },
            "member_dashboard_available": true
        })
    }

    #[test]
    fn spend_limit_is_rendered_with_dollar_figures() {
        let u = parse_usage(&enterprise_response());
        let spend = bucket(&u, "spend").expect("spend bucket should be surfaced");
        assert_eq!(spend.label, "Spend limit");
        assert_eq!(spend.limit_dollars, Some(150.0)); // 15000 minor, exponent 2
        assert_eq!(spend.used_dollars, Some(0.0));
        assert_eq!(spend.remaining_dollars, Some(150.0));
        assert_eq!(spend.utilization, 0.0);
        // No reset timestamp in the response — we must not invent one.
        assert_eq!(spend.resets_at, None);
    }

    #[test]
    fn spend_limit_sorts_above_the_one_time_credit() {
        let u = parse_usage(&enterprise_response());
        let keys: Vec<&str> = u.buckets.iter().map(|b| b.key.as_str()).collect();
        // Plan limit first, then the credit — matching Claude Code's own panel.
        assert_eq!(keys, vec!["spend", "cinder_cove"]);
    }

    #[test]
    fn spend_is_skipped_when_the_plan_has_no_real_limit() {
        // Personal Pro/Max shape: rate-limit buckets, spend disabled and limitless.
        let v = json!({
            "five_hour": { "utilization": 10.0, "resets_at": "2026-06-30T17:29:59+00:00" },
            "seven_day": { "utilization": 3.0,  "resets_at": "2026-07-07T06:59:59+00:00" },
            "spend": {
                "used": { "amount_minor": 0, "currency": "USD", "exponent": 2 },
                "percent": 0, "enabled": false
            }
        });
        let u = parse_usage(&v);
        assert!(bucket(&u, "spend").is_none(), "no empty spend row on personal plans");
        let keys: Vec<&str> = u.buckets.iter().map(|b| b.key.as_str()).collect();
        assert_eq!(keys, vec!["five_hour", "seven_day"]);
    }

    #[test]
    fn spend_is_skipped_when_enabled_but_unlimited() {
        let v = json!({
            "spend": {
                "used": { "amount_minor": 500, "currency": "USD", "exponent": 2 },
                "limit": null, "percent": 0, "enabled": true
            }
        });
        assert!(parse_usage(&v).buckets.is_empty());
    }

    #[test]
    fn spend_honors_a_non_two_exponent_currency() {
        let v = json!({
            "spend": {
                "used":  { "amount_minor": 1200,  "currency": "JPY", "exponent": 0 },
                "limit": { "amount_minor": 15000, "currency": "JPY", "exponent": 0 },
                "percent": 8, "enabled": true
            }
        });
        let u = parse_usage(&v);
        let spend = bucket(&u, "spend").expect("spend bucket");
        assert_eq!(spend.limit_dollars, Some(15000.0));
        assert_eq!(spend.used_dollars, Some(1200.0));
        assert_eq!(spend.utilization, 8.0);
    }

    #[test]
    fn spend_is_not_double_reported_by_the_generic_sweep() {
        let u = parse_usage(&enterprise_response());
        assert_eq!(u.buckets.iter().filter(|b| b.key == "spend").count(), 1);
        // The zeroed org pool still stays hidden, as before.
        assert!(bucket(&u, "nimbus_quill").is_none());
    }

    // ----- ChatGPT/Codex -----

    /// The exact live payload captured 2026-09-07 against a business/workspace
    /// account — see docs/usage-api.md -> "Usage endpoint". `rate_limit` is
    /// present but null on this plan; the real quota is `spend_control`.
    fn chatgpt_business_response() -> Value {
        json!({
            "user_id": "user-abc", "account_id": "acct-123", "email": "a@b.com",
            "plan_type": "business",
            "rate_limit": null,
            "code_review_rate_limit": null,
            "additional_rate_limits": null,
            "model_usage": {},
            "credits": { "has_credits": true, "unlimited": false,
                         "overage_limit_reached": false, "balance": null,
                         "approx_local_messages": null, "approx_cloud_messages": null },
            "spend_control": {
                "reached": false,
                "individual_limit": {
                    "source": "workspace_spend_controls",
                    "limit": "600", "used": "94.37393999099731", "remaining": "505.62606000900270",
                    "used_percent": 16, "remaining_percent": 84,
                    "reset_after_seconds": 2054837, "reset_at": 1790812800
                }
            },
            "rate_limit_reached_type": null, "promo": null,
            "rate_limit_reset_credits": { "available_count": 0, "applicable_available_count": 0 }
        })
    }

    /// Synthetic personal-plan shape: `rate_limit` populated, `spend_control`
    /// entirely absent (not just null) — the mirror image of the business shape.
    fn chatgpt_personal_response() -> Value {
        json!({
            "user_id": "user-xyz", "account_id": "acct-999", "email": "solo@example.com",
            "plan_type": "plus",
            "rate_limit": {
                "primary_window": { "used_percent": 42, "reset_after_seconds": 3600, "reset_at": 1790800000 },
                "secondary_window": { "used_percent": 7, "reset_after_seconds": 604800, "reset_at": 1791300000 }
            },
            "credits": { "has_credits": true, "unlimited": true,
                         "overage_limit_reached": false, "balance": null,
                         "approx_local_messages": null, "approx_cloud_messages": null }
        })
    }

    #[test]
    fn spend_control_is_the_real_quota_on_business_accounts() {
        let u = parse_chatgpt_usage(&chatgpt_business_response());
        let sc = bucket(&u, "spend_control").expect("spend_control bucket should be surfaced");
        assert_eq!(sc.label, "Spend limit");
        // Money arrives as STRINGS on this side — must be parsed, not assumed numeric.
        assert_eq!(sc.limit_dollars, Some(600.0));
        assert!((sc.used_dollars.unwrap() - 94.37393999099731).abs() < 1e-9);
        assert!((sc.remaining_dollars.unwrap() - 505.62606000900270).abs() < 1e-9);
        assert_eq!(sc.utilization, 16.0);
        // reset_at is a unix epoch SECOND here, unlike Anthropic's ISO resets_at.
        assert_eq!(resets_at_epoch(sc), Some(1790812800));
    }

    #[test]
    fn rate_limit_null_on_a_business_account_renders_no_rate_limit_buckets() {
        let u = parse_chatgpt_usage(&chatgpt_business_response());
        assert!(bucket(&u, "rate_limit_primary").is_none());
        assert!(bucket(&u, "rate_limit_secondary").is_none());
    }

    #[test]
    fn credits_with_a_null_balance_renders_no_note() {
        // Matches the captured live response exactly: has_credits true, but
        // nothing meaningful (balance null, not unlimited) — render nothing
        // rather than a fabricated "$0.00" or blank meter.
        let u = parse_chatgpt_usage(&chatgpt_business_response());
        assert_eq!(u.credits_note, None);
    }

    #[test]
    fn rate_limit_windows_render_on_a_personal_plan_with_no_spend_control() {
        let u = parse_chatgpt_usage(&chatgpt_personal_response());
        let primary = bucket(&u, "rate_limit_primary").expect("primary window");
        assert_eq!(primary.utilization, 42.0);
        assert_eq!(resets_at_epoch(primary), Some(1790800000));
        let secondary = bucket(&u, "rate_limit_secondary").expect("secondary window");
        assert_eq!(secondary.utilization, 7.0);
        assert!(bucket(&u, "spend_control").is_none());
    }

    #[test]
    fn unlimited_credits_render_as_a_note() {
        let u = parse_chatgpt_usage(&chatgpt_personal_response());
        assert_eq!(u.credits_note.as_deref(), Some("Unlimited credits"));
    }

    #[test]
    fn a_finite_credit_balance_renders_with_two_decimal_places() {
        let mut v = chatgpt_personal_response();
        v["credits"]["unlimited"] = json!(false);
        v["credits"]["balance"] = json!("12.5");
        let u = parse_chatgpt_usage(&v);
        assert_eq!(u.credits_note.as_deref(), Some("$12.50 credit balance"));
    }

    #[test]
    fn overage_reached_is_appended_to_the_credits_note() {
        let mut v = chatgpt_personal_response();
        v["credits"]["overage_limit_reached"] = json!(true);
        let u = parse_chatgpt_usage(&v);
        assert_eq!(u.credits_note.as_deref(), Some("Unlimited credits (overage limit reached)"));
    }

    /// The task's core requirement: a response that carries BOTH shapes at
    /// once must render both, and neither may hide the other — the mistake
    /// this parser exists to avoid (see the module doc comment above).
    #[test]
    fn rate_limit_and_spend_control_both_render_when_both_are_present() {
        let mut v = chatgpt_business_response();
        v["rate_limit"] = json!({
            "primary_window": { "used_percent": 10, "reset_after_seconds": 3600, "reset_at": 1790800000 },
            "secondary_window": null
        });
        let u = parse_chatgpt_usage(&v);
        assert!(bucket(&u, "rate_limit_primary").is_some());
        assert!(bucket(&u, "spend_control").is_some());
    }
}
