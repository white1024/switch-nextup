# Claude usage & auth API (reverse-engineered)

> Portable reference that travels with this repo. Reverse-engineered from the
> Claude Code VSCode extension bundle and **verified live** on 2026-06-30.
> These are private/undocumented endpoints — they can change without notice.

## Credential storage

Claude Code keeps the logged-in account in a single file:

```
~/.claude/.credentials.json
{
  "claudeAiOauth": {
    "accessToken":  "...",        // OAuth bearer, ~108 chars
    "refreshToken": "...",        // ~108 chars
    "expiresAt":    1782847992843, // unix ms
    "scopes":       ["user:inference", "user:profile", ...], // JSON ARRAY (verified 2026-07-01)
    "subscriptionType": "max",
    "rateLimitTier": "default_claude_max_5x"
  }
}
```

Switching accounts = overwrite this file's `claudeAiOauth` with another saved copy.
Claude Code re-reads the file on the next request / restart.

## Usage endpoint (the `/usage` numbers)

```
GET https://api.anthropic.com/api/oauth/usage
Headers:
  Authorization: Bearer <accessToken>
  anthropic-beta: oauth-2025-04-20
  Content-Type:   application/json
```

Verified response shape (trimmed to the fields we use):

```json
{
  "five_hour":        { "utilization": 10.0, "resets_at": "2026-06-30T17:29:59+00:00" },
  "seven_day":        { "utilization": 3.0,  "resets_at": "2026-07-07T06:59:59+00:00" },
  "seven_day_sonnet": { "utilization": 0.0,  "resets_at": null },
  "seven_day_opus":   null,
  "extra_usage": { "is_enabled": false, "monthly_limit": null, "used_credits": null, "utilization": null },
  "limits": [ { "kind": "session", "percent": 10, "resets_at": "...", "is_active": true }, ... ],
  "spend":  { "used": { "amount_minor": 0, "currency": "USD" }, "percent": 0, "enabled": false }
}
```

- `utilization` is **percent used (0–100)**. A bucket is `null` (or `utilization: null`)
  when not applicable for the plan — skip it.
- `resets_at` is an ISO timestamp; may be `null`.
- `spend` is the plan's **real money limit** and IS rendered — see "Spend limit" below.
  `limits[]` is available but unused (it duplicates the buckets above).

### Spend limit — the `spend` object (verified live 2026-09-07)

The plan's actual money cap. (Money figures in the examples below are illustrative, not captured from a live account — the *shape* is what was verified.) Claude Code's own usage panel shows it **above** the
one-time credit as `$0.00 of $200.00 spent · Spend limit · 0% used`. Shaped unlike
every other bucket:

```json
"spend": {
  "used":  { "amount_minor": 0,     "currency": "USD", "exponent": 2 },
  "limit": { "amount_minor": 20000, "currency": "USD", "exponent": 2 },
  "percent": 0, "severity": "normal", "enabled": true,
  "cap": { "money": null, "credits": { "amount_minor": 20000, "exponent": 2 } },
  "disabled_reason": null, "balance": null, "auto_reload": null,
  "can_purchase_credits": false, "can_toggle": false,
  "disclaimer": "Usage credits cover you when you hit your plan limits. …"
}
```

- **`percent`, not `utilization`** (0–100), and money is **minor units + an
  `exponent`**: `amount_minor: 20000, exponent: 2` = **$200.00**. Don't assume
  exponent 2 — divide by `10^exponent`.
- `enabled: false` with **no `limit`** = the plan has no real spend cap (personal
  Pro/Max look like this). Skip it then, or the card grows an empty row.
- **There is NO reset timestamp anywhere in `spend`.** The official panel's
  "Resets Thu, Oct 1, 8:00 AM GMT+8" is *derived client-side* (= start of the next
  UTC month), which assumes a calendar-month cycle we can't confirm — `extra_usage`
  has `daily`/`weekly` fields hinting other cycles exist. `usage.rs` therefore shows
  **no date** for this bucket rather than guessing one.
- `extra_usage.monthly_limit` (`20000`) is the **same** $200 in minor units, and the
  CLI binary's own renderer sits next to the strings `" spent"`, `"% used"`,
  `"Resets "`, `" monthly limit"`, `used_credits`, `monthly_limit`. Because it is the
  same figure, `extra_usage.utilization` tracks `spend.percent` — the frontend
  therefore suppresses its "Extra usage: N% of monthly credits" note whenever the
  spend meter renders, keeping it only for a plan that reports the utilization with
  no spend limit. Note `extra_usage.utilization` is **`null` until credits are
  actually consumed**, which is why it covered for nothing while `spend` was skipped.
- Observed the promotional credit run out (2026-09-07): `cinder_cove` reached
  `utilization: 100` / `used_dollars: 500` and `spend` began accruing
  (`used.amount_minor: 620` = $6.20, `percent: 3`). So the credit is consumed
  **first** and the real limit only starts moving afterwards — with `spend` hidden,
  the card read a flat "100%" as if the account were out of quota entirely.

> **This was a real bug (fixed 2026-09-07):** `spend` used to sit in
> `usage.rs` `NON_BUCKET_KEYS` and was skipped outright, so an Enterprise account
> showed **only** the $500 promotional `cinder_cove` credit at 98% and none of its
> actual $200 limit. `extra_usage.utilization` is `null` on that plan, so the
> extra-usage line rendered nothing either — the real quota was invisible.

### Credit-based plans (Enterprise / Team) — codename buckets (verified 2026-07-01)

An **Enterprise** account returns a very different set: `five_hour`/`seven_day*` are all
`null`, and usage lives in **volatile internal codename buckets** that carry `_dollars`
fields instead of a rate limit. Verified live response (trimmed):

```json
{
  "five_hour": null, "seven_day": null, "seven_day_sonnet": null, "seven_day_opus": null,
  "cinder_cove":  { "utilization": 0.132, "resets_at": "2026-09-28T…", "limit_dollars": 500,   "used_dollars": 0.66, "remaining_dollars": 499.34 },
  "amber_ladder": { "utilization": 0.0,   "resets_at": "2026-09-02T…", "limit_dollars": 10000, "used_dollars": 0.0,  "remaining_dollars": 10000 },
  "extra_usage":  { "is_enabled": true, "monthly_limit": 20000, "currency": "USD", "utilization": null, … },
  "spend": { … }, "limits": [], "member_dashboard_available": false
}
```

- **`cinder_cove`** is what Claude Code labels **"Claude Code and Cowork credit"** with
  subtext **"One-time credit · Expires {date}"**. `utilization` is still a 0–100 percent
  (`used_dollars/limit_dollars*100`); the CLI shows `Math.floor(utilization)% used`.
- `amber_ladder` (a large org-level $ pool) is **not** shown by the CLI's `/usage`.
- **Codenames churn between releases** — also seen: `tangelo`, `iguana_necktie`,
  `nimbus_quill`, `copper_kite`, `juniper_tide`, `seven_day_cowork`,
  `seven_day_omelette`, `omelette_promotional`, `seven_day_oauth_apps`. **Do not
  hard-code only a fixed set.** `usage.rs` keeps a `KNOWN_BUCKETS` label map (friendly
  names, in order) **plus** a generic sweep that surfaces any other object with
  `utilization > 0`, so a rename can't make usage vanish.
- On the 2026-09-07 re-capture every org $ pool (`amber_ladder`, `copper_kite`,
  `juniper_tide`, `tangelo`, `iguana_necktie`) came back `null` and only `cinder_cove`
  carried figures — another reason the real limit has to come from `spend`, which is
  stable and named, rather than from whichever codename happens to be populated.
- **Display order** in `usage.rs` mirrors the official panel: rate-limit buckets →
  `spend` (the real limit) → `cinder_cove` (the one-time credit) → generic sweep.
- Friendly labels the CLI uses (from the binary): `five_hour`→"Current session",
  `seven_day`→"Current week (all models)", `seven_day_sonnet`→"Current week (Sonnet only)",
  `seven_day_opus`→"Current week (Opus only)".
- Re-derive labels/codenames from the **native CLI binary** (the VSCode `extension.js` is
  only a launcher and does NOT contain the `/usage` parser):
  `~/.local/share/claude/versions/<ver>` — `grep -a "Cowork credit"` / `"cinder_cove"`.

## OAuth login / initial authorization (reverse-engineered 2026-07-02 from binary 2.1.197)

This is the flow `claude /login` runs to obtain the tokens in the first place — i.e. what
the in-app **"Add account / Re-login"** button replicates. Reverse-engineered from the
**native CLI binary** `~/.local/share/claude/versions/2.1.197` (`grep -a`).

> **Status: implemented and live-verified 2026-09-09** (`src-tauri/src/login.rs`).
> This section is the spec *and* the reverse-engineering trail, so read it in order — two
> conclusions recorded below were later overturned, and each carries a dated correction
> banner. The settled behaviour is the last one: **"Settled 2026-09-09 — the loopback
> redirect is refused; the app pastes instead"**.

**Protocol**: OAuth 2.0 **Authorization Code + PKCE (S256)**.

**Authorize endpoint** (two, chosen by account type):
- `https://claude.com/cai/oauth/authorize` — **Claude subscription** accounts (Pro / Max /
  Enterprise). ← the one this app cares about.
- `https://platform.claude.com/oauth/authorize` — Anthropic **Console / API** accounts.

**Authorize query params** — the real builder, verbatim from binary 2.1.250
(function `N2t`), in its own order:

```js
M.searchParams.append("code","true"),                       // ALWAYS, both redirect modes
M.searchParams.append("client_id", …),
M.searchParams.append("response_type","code"),
M.searchParams.append("redirect_uri", o ? MANUAL_REDIRECT_URL : "http://localhost:<port>/callback"),
M.searchParams.append("scope", L.join(" ")),
M.searchParams.append("code_challenge", e),
M.searchParams.append("code_challenge_method","S256"),
M.searchParams.append("state", t),
if (g) M.searchParams.append("orgUUID", g);                 // optional
if (E) M.searchParams.append("login_hint", E);              // optional
if (C) M.searchParams.append("login_method", C);            // optional
```

> ⚠️ **`code=true` is UNCONDITIONAL** — appended first, for the loopback path as well as
> the manual-paste one. An earlier version of this doc claimed the opposite (see the
> corrected note at the end of this section); omitting it is half of why authorization
> failed. There is no `prompt` param in this builder.

- `client_id` = `9d1c250a-e61b-44d9-88ed-5944d1962f5e` (the Claude Code client — same one
  our token refresh uses). A second id `59637612-477b-4836-a601-b0589eda7704` also appears
  (likely the Console/platform client — unconfirmed which host it pairs with).
- `scope`: **the authorize set and the stored/refresh set are NOT the same list.**
  - **authorize** sends `Fvn = dedupe([org:create_api_key, user:profile] + Tq)` =
    `org:create_api_key user:profile user:inference user:sessions:claude_code
    user:mcp_servers user:file_upload` — note `org:create_api_key` **is** part of the
    Claude Code login, contrary to what this doc used to say.
  - **refresh / stored** sends `Tq = user:profile user:inference user:sessions:claude_code
    user:mcp_servers user:file_upload` (no `org:create_api_key`). A logged-in CC has
    exactly these five in `.credentials.json` → `scopes`, i.e. the server does not grant
    the api-key scope, and CC stores the granted set straight from the response.
  - Not part of the login: `user:design:read/write`, `user:projects:read/write`,
    `user:plugins`.
- `code_challenge` = `base64url(SHA256(code_verifier))`; keep the `code_verifier` for the
  token exchange.

**Two redirect modes** (the binary supports both):
- **Loopback** (best for a GUI app): `redirect_uri = http://localhost:<port>/callback`.
  Run a tiny local HTTP server, open the system browser, catch `GET /callback?code=…&state=…`,
  verify `state`. No pasting.
- **Manual paste** (fallback / headless): `redirect_uri =
  https://platform.claude.com/oauth/code/callback`. That page shows a `code#state` string;
  the CLI prompts `Paste code here if prompted >` and does `str.split("#")` → `{code, state}`.
  Success page: `https://platform.claude.com/oauth/code/success?app=claude-code`.

**Token exchange** — same endpoint as refresh, `authorization_code` grant:

```
POST https://platform.claude.com/v1/oauth/token
Headers: Content-Type: application/json
Body:
{
  "grant_type":    "authorization_code",
  "code":          "<code from redirect/paste>",
  "state":         "<state>",                 // Claude sends state on the exchange too
  "redirect_uri":  "<same redirect_uri used above>",
  "client_id":     "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
  "code_verifier": "<the PKCE verifier>"
}
```

**The response returns tokens *and* the account identity** — so a single exchange yields
everything the account store needs (no separate `/roles` call for the core identity). From
the binary's response parsing:

```js
{ accessToken, refreshToken, expiresAt, scopes, clientId,
  subscriptionType, rateLimitTier,
  account:      { uuid, email_address },   // -> oauthAccount.accountUuid / .emailAddress
  organization: { uuid } }                 // -> oauthAccount.organizationUuid
```

So we can write **both** files straight from the exchange result: the `claudeAiOauth`
tokens for `~/.claude/.credentials.json`, and an `oauthAccount` identity
(`accountUuid`, `emailAddress`, `organizationUuid`) for `~/.claude.json` / our stored
account's `identity`. The **richer** `oauthAccount` fields CC keeps (`organizationName`,
`organizationRole`, `subscriptionType`, `rateLimitTier`, `seatTier`,
`claudeCodeTrialEndsAt`, …) come from a **separate profile fetch** the CLI merges in
(`profileFetchedAt`); they are optional for switching (the switch keys off `accountUuid`)
and our `list_accounts` active-detection already auto-upgrades the stored identity, so an
initial login can store just the core identity.

**Logout / revoke**: `POST https://platform.claude.com/v1/oauth/token/revoke` with
`{ token, token_type_hint: "refresh_token", client_id }`.

**Env override**: `CLAUDE_CODE_CUSTOM_OAUTH_URL` can repoint the base, but the binary
validates it (`"CLAUDE_CODE_CUSTOM_OAUTH_URL is not an approved endpoint."`).

> ~~**Confirmed live (2026-08-31)**: the loopback redirect_uri IS whitelisted — an authorize
> URL with `redirect_uri=http://localhost:<port>/callback` for the `9d1c250a` client reaches
> a normal `claude.ai` login/consent page, no `invalid_redirect_uri` error.~~
>
> ❌ **Overturned 2026-09-08/09 — this was the session-costing mistake.** Reaching the
> consent page is **not** evidence the redirect is accepted: claude.ai renders the screen,
> the user presses Allow, and *only then* does it fail with "Authorization failed / Invalid
> request format". The refusal happens **at the moment the code would be issued**, so a
> loopback listener never receives anything. The shipped implementation
> (`src-tauri/src/login.rs`) therefore uses the **manual paste** flow, with no
> listener at all — see "Settled 2026-09-09" below. Generalised lesson: *"the login page
> loaded" is never evidence that an OAuth flow works.*

### ❌ Corrected 2026-09-08 — two wrong claims that cost a session

The 2026-08-31 note in this section also concluded that **no `code=true` param is needed
for the loopback path**, and that the "Authorization failed" the app got back was
**Anthropic-side throttling**. Both were wrong, and the app carried the bug until
2026-09-08, when the user reported `+ Add account` still failing with **"Authorization
failed"** — reproducible, so throttling was never the cause.

Re-reading binary **2.1.250** settled it. Our authorize request differed from the real
client's in two ways:

| | ours (broken) | real client |
|---|---|---|
| `code` param | *absent* | `code=true`, **unconditional**, first param |
| `scope` | 5 scopes, no `org:create_api_key` | 6, led by `org:create_api_key` |

**Why the original check passed anyway** — and the lesson: the 08-31 verification only
established that the authorize URL *reaches a login page*. claude.ai renders that page
before validating the full request, so **everything downstream of consent was untested**.
"The login page loaded" is not evidence that authorization succeeds; only a completed
redirect back to the loopback is. Do not accept a page-loads check as verification of an
OAuth flow again.

**Also settled from the binary** (no longer "to confirm"), from `formatTokens` — the real
client's own parse of the exchange response:

```js
{ accessToken: e.access_token, refreshToken: e.refresh_token,
  expiresAt: Date.now() + e.expires_in*1000,
  refreshTokenExpiresAt: <from e.refresh_token_expires_in>,
  scopes: e.scope.split(" "),
  subscriptionType: <from the profile fetch>, rateLimitTier: <from the profile fetch>,
  tokenAccount: e.account ? { uuid: e.account.uuid, emailAddress: e.account.email_address,
                              organizationUuid: e.organization?.uuid,
                              organizationName: e.organization?.name,
                              workspaceId: e.workspace?.id, workspaceName: e.workspace?.name }
                          : undefined }        // <- NOTE: optional
```

- The response is **snake_case** (`access_token`, `refresh_token`, `expires_in`,
  `refresh_token_expires_in`, `scope` as a **space-joined string**). The camelCase
  fallback in `login.rs` is harmless but not what the wire sends.
- **`account` is OPTIONAL** — `e.account ? {...} : undefined`. `login.rs` used to
  *hard-fail* ("token exchange response has no account identity") when it was absent, which
  would have been the very next failure after the authorize fix. It now merges in the
  profile fetch instead.
- `subscriptionType` / `rateLimitTier` do **not** come from the exchange at all: the client
  calls `GET {BASE_API_URL}/api/oauth/profile` (Bearer + `Content-Type: application/json`)
  after every login and maps `organization.organization_type` through
  `claude_max→max, claude_pro→pro, claude_enterprise→enterprise, claude_team→team`, taking
  `rateLimitTier` from `organization.rate_limit_tier`. The same response carries
  `account.uuid` / `account.email_address` / `organization.uuid` / `organization.name`,
  which is what makes it a usable identity fallback.

Fixed in `login.rs` 2026-09-08 with 5 unit tests, including one asserting the authorize
query matches the real client's parameter set (proved non-vacuous: dropping `code=true`
turns it red). That fix was necessary but **not sufficient** — see below.

### ✅ Settled 2026-09-09 — the loopback redirect is refused; the app pastes instead

With the authorize request byte-identical to the real client's, claude.ai still failed —
but **only after the user pressed Allow**: a real consent screen renders, then
"Authorization failed / Invalid request format", and the browser never returns to the
listener. That timing is the whole diagnosis: **the request is valid; the refusal is at
the moment the code would be issued**, i.e. the loopback `redirect_uri` is not accepted
for the Claude Code client. Both earlier theories (throttling, wrong params) are closed.

**This is a supported path, not a workaround.** The real client builds *both* URLs and is
written to fall back to pasting exactly when its own listener gets nothing:

```js
n = N2t({...o, isManual:true }),   // manual URL — displayed to the user
h = N2t({...o, isManual:false}),   // loopback URL — opened in the browser
… await e(n), await Tr(h)
l = this.authCodeListener?.hasPendingResponse() ?? false;
… useManualRedirect: !l            // no callback arrived -> exchange with the manual redirect
```

So `login.rs` uses the **manual redirect**, which for client `9d1c250a` is registered as:

```
https://platform.claude.com/oauth/code/callback        # NO query string
```

Cross-checked against the official **openai/anthropic-cli** Go source, which documents the
rule that made this worth pinning down: the manual `redirect_uri` must equal the
registered value **verbatim, including any query string** — `anthropic-cli` registers
`/oauth/code/callback?app=anthropic-cli` and an authorize call without that suffix is
rejected with "redirect_uri not supported by client". Claude Code's prod config registers
the bare path (`MANUAL_REDIRECT_URL` in the binary's config object), so no suffix here.
That repo also confirms `state` is **required on the token exchange** (omitting it returns
400 `oauth_request_parse_error`), and notably sends **no `code=true` at all** — that param
belongs to the Claude Code client, not to every Anthropic OAuth client.

Two further bugs had to be fixed before a login completed end to end:

- **Identity must come from `/api/oauth/profile`, not from the token response.** The
  profile's `account.uuid` is the same value Claude Code stores as
  `oauthAccount.accountUuid` (verified live against a real account), and that is what
  `find_matching_account` keys off. With the token response winning, re-logging into an
  **already-saved** account asked for a new name instead of refreshing it in place.
- **`LoginOutcome` serialized as `{"status":"Saved"}`** while `main.js` branches on
  `"saved"`. `#[serde(tag = "status")]` emits variant names verbatim; without
  `rename_all = "snake_case"` the frontend match never fires, so even a fully successful
  login fell through to the "Name this account" prompt. `cargo test` and `node --check`
  are both green while that is broken — the same class of IPC bug — so the wire
  tags are now asserted by a test.

**Live-verified 2026-09-09** with real accounts: an already-saved account re-authorized in
place (no prompt), and an unsaved one stored under a new name. The pasted value may be
`code#state`, a bare code, or the whole callback URL.

## Token refresh (when `expiresAt` is near/past)

```
POST https://platform.claude.com/v1/oauth/token
Headers: Content-Type: application/json
Body:
{
  "grant_type":    "refresh_token",
  "refresh_token": "<refreshToken>",
  "client_id":     "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
  "scope":         "<space-joined scopes>"
}
->
{ "access_token": "...", "refresh_token": "...", "expires_in": <seconds> }
```

> **`scope` gotcha (bit us):** the stored `scopes` is a JSON **array**, but
> this `scope` field must be the array **joined by spaces** into one string. Reading it
> with `.as_str()` yields `None` for an array → an empty scope is sent → refresh returns a
> bad/scopeless token and usage fails. Because Claude Code keeps the *active* account's
> token fresh (so it seldom needs our refresh) while idle *non-active* accounts always do,
> this manifested as "**only non-active accounts show a usage error**". Join the array
> (`arr.join(" ")`); tolerate a plain string too.

Persist the new `access_token` / `refresh_token` and `expiresAt = now_ms + expires_in*1000`
back into the account file (and the live credentials file if that account is active).

## How to re-derive if these break

The `/usage` **parser** and the **login flow** live in the **native CLI binary**, NOT the
VSCode `extension.js` (that's only a launcher). Binary-as-text grep it:

```
~/.local/share/claude/versions/<ver>        # grep -a  (binary-as-text)
```

- **Usage**: `/api/oauth/usage`, `oauth-2025-04-20`, `cinder_cove` / `Cowork credit`, and
  the parser around `five_hour` / `seven_day`.
- **Login / OAuth**: `oauth/authorize`, `code_challenge_method`, `redirect_uri`,
  `searchParams.append` (the authorize param list), `Paste code here if prompted`,
  `oauth/code/callback`, `oauth/code/success`, `oauthAccount`, `.account.uuid` /
  `.organization.uuid` (the token-response identity), `/v1/oauth/token/revoke`.
  Constants (endpoints, client_ids, scopes) sit in a string table; the URL-building code is
  a separate offset — dump ~1–2 KB around a match and `tr -c '[:print:]'` to read the
  minified JS.
- **Refresh / token**: `grant_type`, `BASE_API_URL`, `CLIENT_ID`, `TOKEN_URL`.

The old VSCode bundle path (`~/.vscode/extensions/anthropic.claude-code-<version>/extension.js`)
only has launcher glue — don't expect the parser or the OAuth flow there.

---

# ChatGPT / Codex usage & auth API (reverse-engineered)

> Second provider, added 2026-09-07. **Verified live** the same day against a real
> `business` account on the dev machine. Same caveat as the Claude endpoints above:
> private and undocumented, can change without notice.

## Credential storage

Codex keeps the logged-in account in a single plaintext file:

```
~/.codex/auth.json
{
  "auth_mode": "chatgpt",
  "OPENAI_API_KEY": null,
  "tokens": {
    "id_token":      "<JWT, ~1.9 KB>",
    "access_token":  "<JWT, ~1.9 KB>",
    "refresh_token": "<~200 chars>",
    "account_id":    "<uuid>"
  },
  "last_refresh": "2026-09-07T03:33:01.361101900Z"
}
```

- **Switching is simpler than Claude's: ONE file.** The identity travels *inside*
  the `id_token`, so there is no second identity file to keep in sync (Claude needs
  `~/.claude.json` → `oauthAccount` as well, and forgetting it was the root cause of
  "switch didn't take" — see notes.md).
- **Codex CLI and the Codex desktop app share `~/.codex`** (`.codex-global-state.json`
  carries the Electron desktop's keys), so one swap covers both.
- **OAuth login and refresh ARE available** (settled 2026-09-09 from the `openai/codex`
  source `codex-rs/login/` and the codex binary installed on this machine, which agree):
  issuer `https://auth.openai.com`, authorize `/oauth/authorize`, token `/oauth/token`,
  revoke `/oauth/revoke`; `client_id` `app_EMoamEEZ73f0CkXaXp7hrann`; scopes
  `openid profile email offline_access api.connectors.read api.connectors.invoke`; PKCE
  S256 plus `id_token_add_organizations=true`, `codex_cli_simplified_flow=true`,
  `originator`, `state`, optional `allowed_workspace_id`. The redirect is
  `http://localhost:1455/auth/callback` — a **FIXED** port (`DEFAULT_PORT` 1455,
  `FALLBACK_PORT` 1457), which is very likely why loopback works here and not for Claude
  Code's ephemeral-port client. A device-code flow (`run_device_code_login`) exists too.
  Earlier notes claiming "no known ChatGPT refresh/login endpoint" are superseded.
  **Implemented and live-verified 2026-09-09** in `src-tauri/src/login_codex.rs`.
  Details that cost time to establish:
  - the token exchange is **form-encoded** (`grant_type=authorization_code&code=…&
    redirect_uri=…&client_id=…&code_verifier=…`), *not* the JSON body Anthropic's endpoint
    takes, and the response is `{ id_token, access_token, refresh_token }`
  - `originator` is `codex_cli_rs`
  - the persisted file is `{ "OPENAI_API_KEY": null, "auth_mode": "chatgpt", "tokens":
    { id_token, access_token, refresh_token, account_id }, "last_refresh": <RFC3339> }`;
    `tokens.account_id` is the id_token's `chatgpt_account_id` claim, so derive it rather
    than parsing the JWT twice
  - the browser also hits the loopback with favicon/prefetch requests: answer anything that
    is not `/auth/callback` and keep waiting, or a stray request looks like a failed login
  - the refresh grant on the same endpoint is available but **not used yet**.
- `last_refresh` moves, i.e. **Codex rotates its tokens too**. The Claude-side rule
  applies unchanged: fold the outgoing account's *live* tokens back into its stored
  copy before switching away, or switching back installs a dead token and logs the
  user out. See notes.md → Gotchas ("Session expired" on the account you are using).

### Identity, free of any network call

Base64url-decode the **payload segment** of `id_token` (do NOT verify the signature —
this is our own local file, we are not authenticating anyone):

```
email, name, email_verified, auth_provider, sub, aud, iss, iat, exp, sid, ...
"https://api.openai.com/auth": {
  chatgpt_account_id, chatgpt_plan_type,            // e.g. "business"
  chatgpt_subscription_active_start / _active_until / _last_checked,
  chatgpt_user_id, user_id, organizations, groups, sso_connection_id
}
```

`tokens.account_id` equals the claim's `chatgpt_account_id` — that is the stable id to
match accounts on, the analogue of Claude's `accountUuid`.

## Usage endpoint (the quota numbers)

```
GET https://chatgpt.com/backend-api/wham/usage
Headers:
  Authorization:      Bearer <tokens.access_token>
  ChatGPT-Account-Id: <tokens.account_id>
  originator:         codex_vscode
  User-Agent:         codex_vscode/<codex version> (Windows <ver tag>; <arch>) unknown (VS Code; 0.4.71)
  accept:             */*
  accept-language:    *
  sec-fetch-mode:     cors
```

Queryable **on demand, per account, from a stored token** — no probe request, no quota
spent. Same shape of capability as Anthropic's `/api/oauth/usage`, so the Claude-side
architecture transfers directly (live-fetch the active account only, serve the rest
from the disk cache).

Verified response (values redacted; this is the `business` shape):

```json
{
  "user_id": "user-…", "account_id": "<uuid>", "email": "…",
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
}
```

### Parse all three shapes — a single-shape parser ships blind

1. **`rate_limit.primary_window` / `secondary_window`** → `{ used_percent,
   reset_after_seconds, reset_at }`. Populated on personal plans; **`null` on this
   business account**.
2. **`spend_control.individual_limit`** → the **real quota on business/workspace
   accounts** ($600 limit, $94.37 used, 16% here). Also carries `spend_control.reached`.
3. **`credits`** → `has_credits` / `unlimited` / `balance`; render only when meaningful.

> **This is exactly the same mistake as on the Claude side, one vendor over.** On the Claude side the real
> money limit sat in `spend` while only the rate-limit-style buckets were parsed, and an
> Enterprise account's actual $200 cap never rendered. Here the same trap is waiting:
> `github.com/isxlan0/Codex_AccountSwitch` (the C++ project this endpoint was found via)
> hard-requires `rate_limit`, bails with `rate_limit_missing`, and handles none of
> `spend_control` / `credits` / `model_usage` — so it displays nothing at all on a
> business account like this one. Write the parser generically from the first commit.

**Type traps that differ from the Anthropic side:**
- Money arrives as **strings** (`"600"`, `"94.37393999099731"`), not numbers.
- `reset_at` is a **unix epoch second**, not an ISO timestamp like Anthropic's `resets_at`.
- `used_percent` is already 0–100 (integer), like Anthropic's `utilization`.

## How this was found (and how to re-derive it)

Grepping the Codex binary (`%LOCALAPPDATA%\Programs\OpenAI\Codex\bin\codex.exe`,
`grep -a`) turns up `rate_limits`, `used_percent`, `window_minutes`,
`RateLimitWindow`, `CreditsSnapshot`, `x-codex-credits-balance`,
`x-codex-active-limit`, and the base `https://chatgpt.com/backend-api` — but the
account-quota **path** only appears as the prefix of
`/wham/usage/thread-estimates/query`. Reading that as "thread cost estimation only"
and stopping there is how this endpoint was initially, wrongly, declared absent.
**Lesson: when a grep yields a path prefix, probe the bare prefix before concluding
the capability does not exist.** Codex also persists none of its quota data locally
(`.codex-global-state.json` and every sqlite/WAL store grepped, zero hits), so there
is no on-disk source to read instead — the endpoint is the only way.
