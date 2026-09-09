use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use crate::provider::{self, Provider};

/// Atomically write `contents` to `path`: write a sibling temp file, then rename
/// it over the target. The rename is atomic on the same volume, so a reader
/// (e.g. a running Claude Code) never sees a half-written file, and a write that
/// loses a race can't leave the target truncated. This matters most for the two
/// live files a running Claude Code also writes (`~/.claude/.credentials.json`
/// and `~/.claude.json`): a plain `fs::write` truncates-then-writes in place, so
/// a crash or a Windows sharing conflict mid-write could corrupt the file or,
/// worse, swap one file but not the other — a half-switched account. Mirrors the
/// reference project's temp-file-plus-`os.replace` writes.
pub(crate) fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("path {} has no parent directory", path.display()))?;
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("tmp");
    // Same-directory temp name so the rename stays on one volume (atomic). The
    // pid + counter keep concurrent writers from colliding on the temp file.
    let tmp = parent.join(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    if let Err(e) = fs::write(&tmp, contents) {
        let _ = fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("writing temp file for {}", path.display()));
    }
    fs::rename(&tmp, path).or_else(|e| {
        let _ = fs::remove_file(&tmp);
        Err(e).with_context(|| format!("replacing {}", path.display()))
    })
}

/// Root directory for this app's data: ~/.switch-nextup. `SWITCH_NEXTUP_HOME`
/// overrides it (test-only seam so destructive-path tests use a disposable
/// temp dir instead of the real one — see the `tests` module below; never set
/// in normal use).
///
/// The resolved path is cached for the process, because resolving it can move
/// the pre-rename directory into place (see `resolve_base_dir`) and that is a
/// one-time job, not something to re-check on every call.
pub fn base_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("SWITCH_NEXTUP_HOME") {
        return Ok(PathBuf::from(dir));
    }
    static RESOLVED: OnceLock<PathBuf> = OnceLock::new();
    if let Some(dir) = RESOLVED.get() {
        return Ok(dir.clone());
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow!("cannot resolve home directory"))?;
    Ok(RESOLVED.get_or_init(|| resolve_base_dir(&home)).clone())
}

/// Pick the data directory, moving the pre-rename one into place if it is still
/// there. `~/.claude-switch` is where this app kept everything (accounts, usage
/// cache, window position) while it was called Claude Switch, so an existing
/// install must keep its saved accounts across the rename.
///
/// The move only happens when the new directory does **not** exist yet, so it
/// can never overwrite live data; and if the rename fails — a file inside still
/// held open by another process — we keep using the legacy directory for this
/// process rather than silently coming up with an empty account store, which
/// would look to the user like every saved account had been lost.
fn resolve_base_dir(home: &Path) -> PathBuf {
    let target = home.join(".switch-nextup");
    let legacy = home.join(".claude-switch");
    if target.exists() || !legacy.is_dir() {
        return target;
    }
    match fs::rename(&legacy, &target) {
        Ok(()) => target,
        Err(_) => legacy,
    }
}

/// Root of the account store. Individual accounts live in a **per-provider
/// subdirectory** below this (`accounts/claude/`, `accounts/chatgpt/`) so a Claude
/// and a ChatGPT account may share a name. Files written by the single-provider
/// builds sit loose in this directory and are still read — as Claude — by
/// `legacy_account_file`; see `read_stored`.
fn accounts_root() -> Result<PathBuf> {
    Ok(base_dir()?.join("accounts"))
}

fn accounts_dir(p: Provider) -> Result<PathBuf> {
    Ok(accounts_root()?.join(p.slug()))
}

fn config_path() -> Result<PathBuf> {
    Ok(base_dir()?.join("config.json"))
}

fn usage_root() -> Result<PathBuf> {
    Ok(base_dir()?.join("usage"))
}

/// Per-account usage-cache file, provider-scoped to match the account store.
/// Mirrors `usage.rs`'s `usage_cache_file` location so remove/rename/clear can
/// drop it too and not leave an orphan behind.
pub fn usage_file(p: Provider, name: &str) -> Result<PathBuf> {
    if !is_valid_account_name(name) {
        return Err(anyhow!("invalid account name"));
    }
    Ok(usage_root()?.join(p.slug()).join(format!("{name}.json")))
}

/// Where a pre-multi-provider build cached Claude usage: `usage/<name>.json`,
/// with no provider subdirectory. Read-only fallback so upgrading doesn't blank
/// the "As of …" numbers on every card.
pub fn legacy_usage_file(name: &str) -> Result<PathBuf> {
    if !is_valid_account_name(name) {
        return Err(anyhow!("invalid account name"));
    }
    Ok(usage_root()?.join(format!("{name}.json")))
}

fn ensure_dirs() -> Result<()> {
    for p in Provider::ALL {
        fs::create_dir_all(accounts_dir(p)?)?;
    }
    Ok(())
}

/// Rejects names that would escape the store directory or collide with the
/// `.json` suffix. Shared by the account and usage-cache paths so they can never
/// disagree about what a legal name is.
fn is_valid_account_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(['/', '\\', ':', '.'])
}

fn account_file(p: Provider, name: &str) -> Result<PathBuf> {
    if !is_valid_account_name(name) {
        return Err(anyhow!("invalid account name"));
    }
    Ok(accounts_dir(p)?.join(format!("{name}.json")))
}

/// Where a pre-multi-provider build stored an account: loose in `accounts/`.
/// Those files are Claude accounts by definition (it was the only provider), and
/// they are read but never written — a write goes to the provider subdirectory,
/// which is what migrates them.
fn legacy_account_file(name: &str) -> Result<PathBuf> {
    if !is_valid_account_name(name) {
        return Err(anyhow!("invalid account name"));
    }
    Ok(accounts_root()?.join(format!("{name}.json")))
}

/// The active-account pointer, one per provider.
///
/// `active` is the original single-provider field and is still written, mirroring
/// the Claude pointer, so an older build installed over this one keeps working.
/// `active_by_provider` is the real record, keyed by `Provider::slug()`.
#[derive(Serialize, Deserialize, Default)]
struct Config {
    active: Option<String>,
    #[serde(default)]
    active_by_provider: std::collections::BTreeMap<String, String>,
}

impl Config {
    fn active_for(&self, p: Provider) -> Option<String> {
        if let Some(name) = self.active_by_provider.get(p.slug()) {
            return Some(name.clone());
        }
        // Fall back to the legacy field, which only ever meant Claude.
        match p {
            Provider::Claude => self.active.clone(),
            _ => None,
        }
    }

    fn set_active_for(&mut self, p: Provider, name: Option<&str>) {
        match name {
            Some(n) => {
                self.active_by_provider.insert(p.slug().to_string(), n.to_string());
            }
            None => {
                self.active_by_provider.remove(p.slug());
            }
        }
        if p == Provider::Claude {
            self.active = name.map(String::from);
        }
    }
}

fn read_config() -> Config {
    config_path()
        .ok()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write_config(c: &Config) -> Result<()> {
    ensure_dirs()?;
    atomic_write(&config_path()?, serde_json::to_string_pretty(c)?.as_bytes())?;
    Ok(())
}

// ----- Window position (restored across app restarts) -----
// Kept in its own file (not the accounts config) so window state can never
// interfere with account data. Only the top-left position is persisted — the
// window is a fixed-size frameless widget, so its size stays as configured.

fn window_state_path() -> Result<PathBuf> {
    Ok(base_dir()?.join("window.json"))
}

#[derive(Serialize, Deserialize, Clone, Copy)]
pub struct WindowPos {
    pub x: i32,
    pub y: i32,
}

/// Last saved window position, if any.
pub fn read_window_pos() -> Option<WindowPos> {
    let raw = fs::read_to_string(window_state_path().ok()?).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Persist the window's top-left position (best-effort; never fatal).
pub fn save_window_pos(x: i32, y: i32) {
    if let Ok(p) = window_state_path() {
        if let Ok(s) = serde_json::to_string_pretty(&WindowPos { x, y }) {
            let _ = atomic_write(&p, s.as_bytes());
        }
    }
}

/// Read the live credential blob for a provider (Claude's `claudeAiOauth`
/// object, or Codex's whole `auth.json`). See `provider::read_live`.
pub fn read_live_oauth(p: Provider) -> Result<Value> {
    provider::read_live(p)
}

/// Write a credential blob back into the provider's live credentials file.
pub fn write_live_oauth(p: Provider, oauth: &Value) -> Result<()> {
    provider::write_live(p, oauth)
}

/// Live credentials file that Claude Code writes on login: ~/.claude.json.
fn claude_json_path() -> Result<PathBuf> {
    provider::claude_json_path()
}

/// The live identity for any provider.
///
/// Claude keeps it in a separate file (`~/.claude.json` -> `oauthAccount`);
/// ChatGPT carries it inside the live `id_token`, so it is derived from the blob
/// and there is nothing extra to read. Returns None when it cannot be determined
/// — callers must then not guess and not overwrite anything.
pub fn live_identity(p: Provider) -> Option<Value> {
    match p {
        Provider::Claude => read_live_claude_identity(),
        Provider::Chatgpt => {
            let blob = provider::read_live(p).ok()?;
            provider::identity_from_blob(p, &blob)
        }
    }
}

/// Write an identity back so the provider's own client uses that account.
///
/// Claude needs this — the tokens alone leave Claude Code on the old identity,
/// the original "switch didn't take" bug. ChatGPT needs nothing: the identity is
/// inside the token we already wrote, so there is no second file to keep in sync.
fn write_live_identity(p: Provider, identity: &Value) -> Result<()> {
    match p {
        Provider::Claude => write_live_oauth_account(identity),
        Provider::Chatgpt => Ok(()),
    }
}

/// Whether a stored identity is complete enough to write back on switch.
/// Claude needs a full `oauthAccount` (legacy stubs are not safe to write into
/// `~/.claude.json`); ChatGPT writes no identity file at all, so nothing to check.
fn is_writable_identity(p: Provider, identity: &Value) -> bool {
    match p {
        Provider::Claude => is_full_oauth_account(identity),
        Provider::Chatgpt => true,
    }
}

/// Stable identity of the account currently logged into Claude Code: the full
/// `oauthAccount` object from `~/.claude.json`. Unlike the OAuth tokens (which
/// rotate on refresh), `accountUuid` survives token rotation and only changes on
/// a real login — so it tells "same account, refreshed token" apart from "a
/// different account was logged in", and never clobbers a stored account. We keep
/// the *whole* object (not just uuid+email) because a switch must write it back
/// into `~/.claude.json` so Claude Code's active identity matches the swapped
/// tokens — see `write_live_oauth_account` / `switch`.
fn read_live_claude_identity() -> Option<Value> {
    let raw = fs::read_to_string(claude_json_path().ok()?).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    let acc = v.get("oauthAccount")?;
    if !acc.is_object() {
        return None;
    }
    // Require at least one identifying field so we never capture an empty object.
    let has_uuid = acc.get("accountUuid").and_then(|x| x.as_str()).is_some();
    let has_email = acc.get("emailAddress").and_then(|x| x.as_str()).is_some();
    if !has_uuid && !has_email {
        return None;
    }
    Some(acc.clone())
}

/// Whether a stored identity is a full `oauthAccount` object (captured with the
/// identity-swap fix) rather than the legacy `{ accountUuid, email }` stub. Only a
/// full object can be safely written back into `~/.claude.json` on switch; the
/// stub is enough for active detection but not for restoring the identity. The
/// live `oauthAccount` always carries `emailAddress`, which the legacy stub never
/// had (it used `email`), so its presence is the discriminator.
fn is_full_oauth_account(id: &Value) -> bool {
    id.get("emailAddress").is_some()
}

/// Write `oauth_account` into `~/.claude.json` -> `oauthAccount`, preserving every
/// other key in the file. Claude Code reads the active account *identity* from
/// this object (separately from the tokens in `.credentials.json`), so a switch
/// that only swaps the tokens leaves CC on the old account — it must swap this
/// too. Read-modify-write keeps projects/history/settings intact; a missing file
/// is created with just this object (CC backfills the rest on next run).
pub fn write_live_oauth_account(oauth_account: &Value) -> Result<()> {
    let p = claude_json_path()?;
    let mut root: Value = match fs::read_to_string(&p) {
        Ok(raw) => {
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", p.display()))?
        }
        Err(_) => json!({}),
    };
    if !root.is_object() {
        root = json!({});
    }
    root["oauthAccount"] = oauth_account.clone();
    atomic_write(&p, serde_json::to_string_pretty(&root)?.as_bytes())
        .with_context(|| format!("writing {}", p.display()))?;
    Ok(())
}

/// Whether Claude Code on this machine may be reading the active account from
/// **Windows Credential Manager** instead of the plaintext `.credentials.json`
/// file this app swaps — true when the env override is set, or when the
/// server-controlled GrowthBook flag `tengu_windows_credman` in
/// `~/.claude.json` -> `cachedGrowthBookFeatures` is `true`. When true,
/// file-swap switching may silently stop taking effect. See docs/architecture.md ->
/// Gotchas, "Windows credential source".
pub fn credman_guard_active() -> bool {
    if std::env::var("CLAUDE_CODE_FORCE_WINDOWS_CREDMAN").as_deref() == Ok("1") {
        return true;
    }
    claude_json_path()
        .ok()
        .and_then(|p| fs::read_to_string(p).ok())
        .map(|raw| credman_flag_from_claude_json(&raw))
        .unwrap_or(false)
}

/// Parses just the `cachedGrowthBookFeatures.tengu_windows_credman` bool out of
/// a `~/.claude.json`-shaped string. Split out from `credman_guard_active` so
/// the parsing logic is unit-testable without touching the filesystem.
fn credman_flag_from_claude_json(raw: &str) -> bool {
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|v| {
            v.get("cachedGrowthBookFeatures")?
                .get("tengu_windows_credman")?
                .as_bool()
        })
        .unwrap_or(false)
}

/// The stable account id inside an identity object, per provider.
fn identity_uuid(p: Provider, id: &Value) -> Option<String> {
    provider::stable_id(p, id)
}

/// Read a stored account file -> (credential blob, optional captured identity).
/// Format is `{ "oauth": {...}, "identity": {...} }`; a bare blob (legacy) is
/// accepted with no identity. Falls back to the pre-multi-provider location
/// (`accounts/<name>.json`, loose) for Claude, so upgrading never orphans an
/// account the user already saved.
fn read_stored(p: Provider, name: &str) -> Result<(Value, Option<Value>)> {
    let path = account_file(p, name)?;
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(_) if p == Provider::Claude => fs::read_to_string(legacy_account_file(name)?)
            .with_context(|| format!("account '{name}' not found"))?,
        Err(e) => return Err(e).with_context(|| format!("account '{name}' not found")),
    };
    let v: Value = serde_json::from_str(&raw).context("parsing account json")?;
    Ok(split_stored(v))
}

/// Split a parsed account file value into (oauth, identity), tolerating both the
/// new wrapped format and the legacy bare-oauth format.
fn split_stored(v: Value) -> (Value, Option<Value>) {
    if v.get("oauth").is_some() {
        let identity = v.get("identity").cloned().filter(|x| !x.is_null());
        return (v.get("oauth").cloned().unwrap_or(Value::Null), identity);
    }
    (v, None)
}

fn write_stored(p: Provider, name: &str, oauth: &Value, identity: Option<&Value>) -> Result<()> {
    ensure_dirs()?;
    let wrapped = json!({ "oauth": oauth, "identity": identity });
    atomic_write(&account_file(p, name)?, serde_json::to_string_pretty(&wrapped)?.as_bytes())?;
    // A Claude account read from the pre-multi-provider location has now been
    // rewritten into the provider subdirectory; drop the loose original so it
    // cannot come back as a duplicate entry in the list.
    if p == Provider::Claude {
        if let Ok(legacy) = legacy_account_file(name) {
            if legacy.exists() {
                let _ = fs::remove_file(legacy);
            }
        }
    }
    Ok(())
}

/// The stored credential blob for a named account (identity stripped).
pub fn read_account_oauth(p: Provider, name: &str) -> Result<Value> {
    read_stored(p, name).map(|(oauth, _)| oauth)
}

/// The stored identity for a named account, if any.
pub fn read_account_identity(p: Provider, name: &str) -> Option<Value> {
    read_stored(p, name).ok().and_then(|(_, id)| id)
}

/// Persist a credential blob for a name, preserving any already-captured identity.
pub fn write_account_oauth(p: Provider, name: &str, oauth: &Value) -> Result<()> {
    let identity = read_account_identity(p, name);
    write_stored(p, name, oauth, identity.as_ref())
}

/// Save a freshly-obtained in-app login/re-login result under `name`. Unlike
/// `write_account_oauth`, always writes the given identity rather than
/// preserving whatever was already stored — a login result is itself a fresh,
/// full identity capture (see `login.rs`).
pub fn write_login_result(p: Provider, name: &str, oauth: &Value, identity: &Value) -> Result<()> {
    write_stored(p, name, oauth, Some(identity))
}

/// Find a saved account whose captured identity is the SAME Claude account as
/// `identity` (matched by the stable `accountUuid`, which survives token
/// rotation and a full re-login). None when `identity` has no uuid, or no
/// saved account's identity matches — the caller should fall back to asking
/// the user for a name (first save, or an old account with no captured
/// identity). Lets "Save current account" update an existing entry in place
/// after a `claude /login` re-auth, instead of asking the user to name/overwrite
/// it again.
pub fn find_matching_account(p: Provider, identity: &Value) -> Option<String> {
    let uuid = identity_uuid(p, identity)?;
    stored_accounts(p)
        .into_iter()
        .find(|(_, _, id)| {
            id.as_ref().and_then(|i| identity_uuid(p, i)).as_deref() == Some(uuid.as_str())
        })
        .map(|(name, _, _)| name)
}

/// Every stored account for a provider as (name, credential blob, identity),
/// sorted by name. Reads the provider subdirectory plus — for Claude only — the
/// pre-multi-provider loose files, with the subdirectory winning on a name clash
/// so a migrated account never appears twice.
fn stored_accounts(p: Provider) -> Vec<(String, Value, Option<Value>)> {
    let mut found: std::collections::BTreeMap<String, (Value, Option<Value>)> =
        std::collections::BTreeMap::new();

    let mut dirs: Vec<PathBuf> = Vec::new();
    // Legacy first so the provider subdirectory overwrites it.
    if p == Provider::Claude {
        if let Ok(d) = accounts_root() {
            dirs.push(d);
        }
    }
    if let Ok(d) = accounts_dir(p) {
        dirs.push(d);
    }

    for dir in dirs {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let name = match path.file_stem().and_then(|s| s.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            if let Ok(raw) = fs::read_to_string(&path) {
                if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                    let (oauth, identity) = split_stored(v);
                    found.insert(name, (oauth, identity));
                }
            }
        }
    }

    let mut out: Vec<(String, Value, Option<Value>)> = found
        .into_iter()
        .map(|(name, (oauth, id))| (name, oauth, id))
        .collect();
    out.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    out
}

/// The oauth tokens to actually USE for `name` right now.
///
/// Claude Code refreshes `~/.claude/.credentials.json` on its own schedule, and
/// the refresh token **rotates** on every refresh — the previous one dies. Our
/// stored copy is only re-synced inside `list_accounts`, which the frontend calls
/// on startup and on `accounts-changed`; the 120s auto-refresh, focus/visibility
/// regain and the manual refresh button all go straight to `get_usage`. So for
/// the account currently in use, the stored copy can be hours out of date while
/// the live session is perfectly alive — and refreshing with its rotated-away
/// refresh token gets a 4xx that we would report to the user as "session
/// expired" for the very account they are working in.
///
/// So: when the live credentials belong to THIS account, use the live tokens
/// (Claude Code keeps them fresh) and re-sync the stored copy. "Belong to this
/// account" is proven the same way `list_accounts` proves it — an exact token
/// match, or the stable `accountUuid`, which survives rotation. Anything else
/// (a different live account, or no readable live identity) returns the stored
/// copy untouched, so we never adopt or overwrite another account's tokens.
/// (Codex rotates its tokens on its own schedule too — `auth.json`'s
/// `last_refresh` moves — so this applies to ChatGPT accounts identically.)
pub fn resolve_account_oauth(p: Provider, name: &str) -> Result<Value> {
    let (stored, stored_identity) = read_stored(p, name)?;
    let live = match read_live_oauth(p) {
        Ok(live) => live,
        // No readable live credentials file: the stored copy is all we have.
        Err(_) => return Ok(stored),
    };

    // Already the same tokens -> nothing to adopt, nothing to write.
    let live_refresh = provider::refresh_token_of(p, &live);
    let live_access = provider::access_token_of(p, &live);
    if (live_refresh.is_some() && live_refresh == provider::refresh_token_of(p, &stored))
        || (live_access.is_some() && live_access == provider::access_token_of(p, &stored))
    {
        return Ok(stored);
    }

    let live_id = live_identity(p);
    let live_uuid = live_id.as_ref().and_then(|i| identity_uuid(p, i));
    let stored_uuid = stored_identity.as_ref().and_then(|i| identity_uuid(p, i));
    match (live_uuid, stored_uuid) {
        (Some(live_uuid), Some(stored_uuid)) if live_uuid == stored_uuid => {
            // Same account, tokens rotated externally by the provider's own
            // client. Persist so the stored copy stops drifting: a stale copy
            // would also be written back into the live file by `switch`,
            // installing a dead token.
            let _ = write_stored(p, name, &live, live_id.as_ref());
            Ok(live)
        }
        _ => Ok(stored),
    }
}

/// Fold the current live credentials into whichever stored account they belong
/// to, matched by the stable `accountUuid`. Returns the account name it synced,
/// or None when the live account is not one we have saved (in which case nothing
/// is written). Best-effort: callers use it to capture externally-rotated tokens
/// before they are overwritten.
fn sync_live_into_stored_account(p: Provider) -> Option<String> {
    let live = read_live_oauth(p).ok()?;
    let live_id = live_identity(p)?;
    let name = find_matching_account(p, &live_id)?;
    write_stored(p, &name, &live, Some(&live_id)).ok()?;
    Some(name)
}

#[derive(Serialize, Clone)]
pub struct AccountInfo {
    pub name: String,
    /// `Provider::slug()` — the frontend groups its tabs on this.
    pub provider: String,
    pub subscription_type: Option<String>,
    pub rate_limit_tier: Option<String>,
    pub expires_at: Option<i64>,
    /// Shown on the card. Claude reads it from the captured `oauthAccount`;
    /// ChatGPT from the `id_token` claims.
    pub email: Option<String>,
    pub is_active: bool,
}

/// Every provider's accounts in one list, in `Provider::ALL` order. The frontend
/// takes the whole set in a single call and splits it into tabs on `provider`.
pub fn list_all_accounts() -> Result<Vec<AccountInfo>> {
    let mut out = Vec::new();
    for p in Provider::ALL {
        out.extend(list_accounts_for(p)?);
    }
    Ok(out)
}

/// List all stored accounts, marking which one is currently loaded into the
/// live credentials file. Detection matches by refresh/access token, then
/// falls back to the recorded config pointer. When the live token has been
/// refreshed externally (e.g. by Claude Code) for the active account, the
/// stored copy is re-synced so usage calls keep working.
pub fn list_accounts_for(p: Provider) -> Result<Vec<AccountInfo>> {
    ensure_dirs()?;
    // (name, credential blob, identity)
    let mut accounts: Vec<(String, Value, Option<Value>)> = stored_accounts(p);

    let live = read_live_oauth(p).ok();
    let live_refresh = live.as_ref().and_then(|l| provider::refresh_token_of(p, l));
    let live_access = live.as_ref().and_then(|l| provider::access_token_of(p, l));
    let live_identity = live_identity(p);
    let live_uuid = live_identity.as_ref().and_then(|i| identity_uuid(p, i));

    let mut active_name: Option<String> = None;

    // 1) Exact token match (fast path right after a switch, or nothing rotated).
    for (name, oauth, _) in &accounts {
        let r = provider::refresh_token_of(p, oauth);
        let a = provider::access_token_of(p, oauth);
        if (live_refresh.is_some() && r == live_refresh)
            || (live_access.is_some() && a == live_access)
        {
            active_name = Some(name.clone());
            break;
        }
    }

    // Backfill/upgrade identity for a token-matched account: the token match proves
    // live IS this account, so recording its full live `oauthAccount` is safe. This
    // makes future rotations identity-detectable AND upgrades accounts saved with
    // only the legacy uuid+email stub to a full object, so the next switch can write
    // the identity back into `~/.claude.json` without a manual re-save.
    if let Some(name) = active_name.clone() {
        if let (Some(live_oauth), Some(live_id)) = (&live, &live_identity) {
            if let Some(slot) = accounts.iter_mut().find(|(n, _, _)| *n == name) {
                let needs_upgrade = slot
                    .2
                    .as_ref()
                    .map_or(true, |id| !is_writable_identity(p, id));
                if needs_upgrade {
                    let _ = write_stored(p, &name, live_oauth, Some(live_id));
                    slot.2 = Some(live_id.clone());
                }
            }
        }
    }

    // 2) No token match -> the live token was either rotated (same account) or a
    // DIFFERENT account was logged in externally. Use the stable accountUuid to
    // tell them apart. Only re-sync the stored copy when the identity confirms
    // it is the same account; never overwrite a different account's file.
    if active_name.is_none() {
        if let Some(live_uuid) = &live_uuid {
            if let Some(idx) = accounts.iter().position(|(_, _, id)| {
                id.as_ref().and_then(|i| identity_uuid(p, i)).as_deref()
                    == Some(live_uuid.as_str())
            }) {
                let name = accounts[idx].0.clone();
                if let Some(live_oauth) = &live {
                    // Matched by the stable account id -> confirmed the SAME
                    // account, so refresh both the rotated tokens and the
                    // identity (also upgrades a legacy Claude stub).
                    let _ = write_stored(p, &name, live_oauth, live_identity.as_ref());
                    accounts[idx].1 = live_oauth.clone();
                    accounts[idx].2 = live_identity.clone();
                }
                active_name = Some(name);
            }
            // else: the logged-in account is not one we've saved -> nothing active,
            // and crucially nothing overwritten.
        }
        // else: no readable live identity -> do not guess, do not overwrite.
    }

    let mut cfg = read_config();
    if active_name != cfg.active_for(p) {
        cfg.set_active_for(p, active_name.as_deref());
        let _ = write_config(&cfg);
    }

    Ok(accounts
        .into_iter()
        .map(|(name, oauth, identity)| {
            let is_active = Some(&name) == active_name.as_ref();
            AccountInfo {
                provider: p.slug().to_string(),
                subscription_type: provider::plan_of(p, &oauth, identity.as_ref()),
                rate_limit_tier: oauth
                    .get("rateLimitTier")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                expires_at: oauth.get("expiresAt").and_then(|v| v.as_i64()),
                email: identity
                    .as_ref()
                    .and_then(|id| id.get("emailAddress"))
                    .and_then(|v| v.as_str())
                    .map(String::from),
                is_active,
                name,
            }
        })
        .collect())
}

/// Capture the currently-loaded live credentials as a named account.
pub fn capture_current(p: Provider, name: &str) -> Result<()> {
    let oauth = read_live_oauth(p)?;
    let identity = live_identity(p);
    write_stored(p, name, &oauth, identity.as_ref())?;
    let mut cfg = read_config();
    cfg.set_active_for(p, Some(name));
    write_config(&cfg)?;
    Ok(())
}

/// Switch the live credentials to the named account.
///
/// Swaps BOTH files Claude Code keys off: the OAuth tokens in
/// `~/.claude/.credentials.json` AND the account identity in `~/.claude.json` ->
/// `oauthAccount`. Writing only the tokens leaves CC's stored identity on the old
/// account, so on next start (or token refresh) it keeps using / reverts to the
/// old account — the root cause of "switch didn't take". Accounts saved before the
/// identity-swap fix only carry the legacy uuid+email stub; we skip the identity
/// write for those (rather than corrupt `~/.claude.json`) and ask the user to
/// re-save so a full `oauthAccount` gets captured.
/// Only ever touches the ONE provider's live files, so switching a ChatGPT
/// account cannot disturb the active Claude account or vice versa — the two
/// clients read entirely separate files and each has its own active pointer.
pub fn switch(p: Provider, name: &str) -> Result<()> {
    // Before overwriting the live file, fold the *outgoing* account's current
    // live tokens back into its own stored copy. Both Claude Code and Codex
    // rotate them on their own schedule, so without this a switch away and back
    // re-installs whatever we last happened to see — possibly a dead,
    // rotated-away token, which would log the user out. Best-effort: never
    // block the switch.
    let _ = sync_live_into_stored_account(p);

    let oauth = read_account_oauth(p, name)?;
    write_live_oauth(p, &oauth)?;
    match read_account_identity(p, name) {
        Some(id) if is_writable_identity(p, &id) => write_live_identity(p, &id)?,
        // Claude-only path: a pre-identity-fix account carries just the legacy
        // uuid+email stub, which is not safe to write into ~/.claude.json.
        // ChatGPT always reports true here (no identity file to write).
        _ => eprintln!(
            "switch-nextup: {} account '{name}' has no full stored identity \
             (saved before the identity-swap fix); wrote tokens only. Re-save it \
             so switching also updates the account identity.",
            p.slug()
        ),
    }
    let mut cfg = read_config();
    cfg.set_active_for(p, Some(name));
    write_config(&cfg)?;
    Ok(())
}

pub fn remove(p: Provider, name: &str) -> Result<()> {
    for path in [account_file(p, name).ok(), legacy_claude_file_if(p, name)]
        .into_iter()
        .flatten()
    {
        if path.exists() {
            fs::remove_file(path)?;
        }
    }
    // Drop the account's usage cache too, or it lingers as an orphan file.
    for u in [usage_file(p, name).ok(), legacy_usage_if(p, name)]
        .into_iter()
        .flatten()
    {
        let _ = fs::remove_file(u);
    }
    let mut cfg = read_config();
    if cfg.active_for(p).as_deref() == Some(name) {
        cfg.set_active_for(p, None);
        write_config(&cfg)?;
    }
    Ok(())
}

/// The pre-multi-provider account path, but only for Claude — the only provider
/// that can have one. Lets remove/rename clean up a not-yet-migrated file.
fn legacy_claude_file_if(p: Provider, name: &str) -> Option<PathBuf> {
    (p == Provider::Claude).then(|| legacy_account_file(name).ok()).flatten()
}

fn legacy_usage_if(p: Provider, name: &str) -> Option<PathBuf> {
    (p == Provider::Claude).then(|| legacy_usage_file(name).ok()).flatten()
}

pub fn rename(p: Provider, old: &str, new: &str) -> Result<()> {
    let (oauth, identity) = read_stored(p, old)?;
    write_stored(p, new, &oauth, identity.as_ref())?;
    for path in [account_file(p, old).ok(), legacy_claude_file_if(p, old)]
        .into_iter()
        .flatten()
    {
        if path.exists() {
            fs::remove_file(path)?;
        }
    }
    // Move the usage cache along with the account (best-effort). Prefer the
    // provider-scoped file, falling back to a not-yet-migrated legacy one.
    if let Ok(to) = usage_file(p, new) {
        if let Some(parent) = to.parent() {
            let _ = fs::create_dir_all(parent);
        }
        for from in [usage_file(p, old).ok(), legacy_usage_if(p, old)]
            .into_iter()
            .flatten()
        {
            if from.exists() {
                let _ = fs::rename(&from, &to);
                break;
            }
        }
    }
    let mut cfg = read_config();
    if cfg.active_for(p).as_deref() == Some(old) {
        cfg.set_active_for(p, Some(new));
        write_config(&cfg)?;
    }
    Ok(())
}

/// Delete all saved accounts, the active pointer, and cached usage. Keeps the
/// app's other local state (saved window position; UI prefs live in the webview).
/// **Never touches the providers' own files** (`~/.claude/…`, `~/.codex/…`), so it
/// does not log the user out of Claude Code or Codex — only Switch NextUp's
/// stored copies are removed. Removing the roots covers every provider's
/// subdirectory and any not-yet-migrated legacy files in one go.
pub fn clear_accounts() -> Result<()> {
    for dir in [accounts_root()?, usage_root()?] {
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
    }
    let cfg = config_path()?;
    if cfg.exists() {
        fs::remove_file(cfg)?;
    }
    Ok(())
}

/// Full reset: remove the entire `~/.switch-nextup/` directory (accounts, config,
/// usage cache, saved window position). Like `clear_accounts` it never touches
/// `~/.claude/…`, so Claude Code stays logged in. The webview's localStorage prefs
/// and the OS autostart entry are cleared by the caller.
pub fn reset_all() -> Result<()> {
    let base = base_dir()?;
    if base.exists() {
        fs::remove_dir_all(&base)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
    use std::sync::Mutex;

    // SWITCH_NEXTUP_HOME is a process-global env var, so tests that set it must
    // not run concurrently with each other. This lock serializes just those
    // tests; it does not affect any other test that might be added later.
    static ENV_LOCK: Mutex<()> = Mutex::new(());
    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A disposable `~/.switch-nextup` stand-in under the OS temp dir, torn
    /// down on drop. Holds the ENV_LOCK guard for its whole lifetime so no
    /// other test's env var mutation can interleave with this one.
    struct TestHome {
        _guard: std::sync::MutexGuard<'static, ()>,
        dir: PathBuf,
        /// Stands in for the user's home dir as far as Claude Code's own files go
        /// (`<claude_home>/.claude/.credentials.json` + `<claude_home>/.claude.json`),
        /// so a test can fake the live account without reading — let alone
        /// writing — the real ones on the dev machine.
        claude_home: PathBuf,
        /// Stands in for `~/.codex`, so a ChatGPT test never reads — let alone
        /// writes — the developer's real Codex login.
        codex_home: PathBuf,
    }

    impl TestHome {
        fn new(label: &str) -> Self {
            // A failing test panics while holding this guard, which poisons the
        // mutex; taking the inner guard anyway keeps that one failure from
        // cascading into every other test as an unrelated lock panic.
        let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let dir = std::env::temp_dir().join(format!(
                "switch-nextup-test-{label}-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, AtomicOrdering::Relaxed)
            ));
            let claude_home = dir.with_file_name(format!(
                "{}-claude-home",
                dir.file_name().unwrap().to_str().unwrap()
            ));
            let codex_home = dir.with_file_name(format!(
                "{}-codex-home",
                dir.file_name().unwrap().to_str().unwrap()
            ));
            fs::create_dir_all(&dir).unwrap();
            fs::create_dir_all(claude_home.join(".claude")).unwrap();
            fs::create_dir_all(&codex_home).unwrap();
            std::env::set_var("SWITCH_NEXTUP_HOME", &dir);
            std::env::set_var("SWITCH_NEXTUP_CLAUDE_HOME", &claude_home);
            std::env::set_var("SWITCH_NEXTUP_CODEX_HOME", &codex_home);
            TestHome { _guard: guard, dir, claude_home, codex_home }
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            std::env::remove_var("SWITCH_NEXTUP_HOME");
            std::env::remove_var("SWITCH_NEXTUP_CLAUDE_HOME");
            std::env::remove_var("SWITCH_NEXTUP_CODEX_HOME");
            let _ = fs::remove_dir_all(&self.dir);
            let _ = fs::remove_dir_all(&self.claude_home);
            let _ = fs::remove_dir_all(&self.codex_home);
        }
    }

    /// Seed a minimal fake account + config + usage cache directly on disk
    /// (not via write_stored/write_config), so the seed stays independent of
    /// the code under test.
    fn seed_fake_account(dir: &Path, name: &str) {
        let accounts = dir.join("accounts");
        fs::create_dir_all(&accounts).unwrap();
        fs::write(
            accounts.join(format!("{name}.json")),
            r#"{"oauth":{"accessToken":"fake-access","refreshToken":"fake-refresh"},"identity":{"accountUuid":"fake-uuid","emailAddress":"fake@example.com"}}"#,
        )
        .unwrap();
        let usage = dir.join("usage");
        fs::create_dir_all(&usage).unwrap();
        fs::write(usage.join(format!("{name}.json")), r#"{"fetched_at":0,"buckets":[]}"#).unwrap();
        fs::write(dir.join("config.json"), format!(r#"{{"active":"{name}"}}"#)).unwrap();
    }

    // ----- ChatGPT / Codex seeds -----

    /// An unsigned JWT carrying the OpenAI auth claim, matching the real
    /// `id_token` wire shape so identity extraction is exercised for real.
    fn codex_id_token(account_id: &str, email: &str, plan: &str) -> String {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
        let claims = json!({
            "email": email,
            "name": "Test User",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": account_id,
                "chatgpt_plan_type": plan,
            }
        });
        format!(
            "{}.{}.sig",
            URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256"}"#),
            URL_SAFE_NO_PAD.encode(claims.to_string().as_bytes())
        )
    }

    /// The `auth.json` shape Codex writes.
    fn codex_blob(account_id: &str, email: &str, token_tag: &str) -> Value {
        json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": codex_id_token(account_id, email, "business"),
                "access_token": format!("access-{token_tag}"),
                "refresh_token": format!("refresh-{token_tag}"),
                "account_id": account_id
            },
            "last_refresh": "2026-09-07T03:33:01Z"
        })
    }

    /// Seed a stored ChatGPT account straight to disk, in the provider
    /// subdirectory, without going through the code under test.
    fn seed_chatgpt_account(dir: &Path, name: &str, account_id: &str, token_tag: &str) {
        let accounts = dir.join("accounts").join("chatgpt");
        fs::create_dir_all(&accounts).unwrap();
        let blob = codex_blob(account_id, &format!("{name}@example.com"), token_tag);
        let identity = json!({
            "accountId": account_id,
            "emailAddress": format!("{name}@example.com"),
            "planType": "business"
        });
        fs::write(
            accounts.join(format!("{name}.json")),
            serde_json::to_string_pretty(&json!({ "oauth": blob, "identity": identity })).unwrap(),
        )
        .unwrap();
    }

    /// Stand up a fake live `~/.codex/auth.json`.
    fn seed_codex_live(codex_home: &Path, account_id: &str, email: &str, token_tag: &str) {
        fs::create_dir_all(codex_home).unwrap();
        fs::write(
            codex_home.join("auth.json"),
            serde_json::to_string_pretty(&codex_blob(account_id, email, token_tag)).unwrap(),
        )
        .unwrap();
    }

    fn codex_live_token(codex_home: &Path, field: &str) -> String {
        let raw = fs::read_to_string(codex_home.join("auth.json")).unwrap();
        let v: Value = serde_json::from_str(&raw).unwrap();
        v["tokens"][field].as_str().unwrap_or_default().to_string()
    }

    /// Seed a stored account with explicit tokens + identity uuid, written
    /// straight to disk so the seed does not depend on the code under test.
    fn seed_account(dir: &Path, name: &str, uuid: &str, token_tag: &str, expires_at: i64) {
        let accounts = dir.join("accounts");
        fs::create_dir_all(&accounts).unwrap();
        let json = format!(
            concat!(
                r#"{{"oauth":{{"accessToken":"access-{tag}","refreshToken":"refresh-{tag}","#,
                r#""expiresAt":{exp},"scopes":["user:inference"]}},"#,
                r#""identity":{{"accountUuid":"{uuid}","emailAddress":"{name}@example.com"}}}}"#
            ),
            tag = token_tag,
            exp = expires_at,
            uuid = uuid,
            name = name
        );
        fs::write(accounts.join(format!("{name}.json")), json).unwrap();
    }

    /// Seed the fake live files Claude Code owns: tokens in
    /// `.claude/.credentials.json` plus the account identity in `.claude.json`.
    fn seed_live(claude_home: &Path, uuid: &str, token_tag: &str, expires_at: i64) {
        fs::create_dir_all(claude_home.join(".claude")).unwrap();
        let creds = format!(
            concat!(
                r#"{{"claudeAiOauth":{{"accessToken":"access-{tag}","refreshToken":"refresh-{tag}","#,
                r#""expiresAt":{exp},"scopes":["user:inference"]}}}}"#
            ),
            tag = token_tag,
            exp = expires_at
        );
        fs::write(claude_home.join(".claude").join(".credentials.json"), creds).unwrap();
        let claude_json = format!(
            concat!(
                r#"{{"projects":{{"keep":"me"}},"#,
                r#""oauthAccount":{{"accountUuid":"{uuid}","emailAddress":"live@example.com"}}}}"#
            ),
            uuid = uuid
        );
        fs::write(claude_home.join(".claude.json"), claude_json).unwrap();
    }

    fn token_of(v: &Value, field: &str) -> String {
        v.get(field).and_then(|t| t.as_str()).unwrap_or_default().to_string()
    }

    // The bug this guards against: Claude Code refreshes the live token on its
    // own schedule and the refresh token rotates, so the stored copy of the
    // *account in use* goes stale. Using it would refresh with a dead token and
    // report "session expired" for a perfectly live session.
    #[test]
    fn resolve_account_oauth_adopts_rotated_live_tokens_when_the_uuid_matches() {
        let home = TestHome::new("resolve-adopts");
        seed_account(&home.dir, "work", "uuid-same", "stale", 1_000);
        seed_live(&home.claude_home, "uuid-same", "fresh", 9_999_999_999_999);

        let resolved = resolve_account_oauth(Provider::Claude, "work").expect("resolve should succeed");
        assert_eq!(token_of(&resolved, "accessToken"), "access-fresh");
        assert_eq!(token_of(&resolved, "refreshToken"), "refresh-fresh");

        // …and the stored copy is re-synced, so it stops drifting (a stale copy
        // would be written back into the live file by a later switch).
        let stored = read_account_oauth(Provider::Claude, "work").unwrap();
        assert_eq!(token_of(&stored, "accessToken"), "access-fresh");
        assert_eq!(
            read_account_identity(Provider::Claude, "work")
                .as_ref()
                .and_then(|id| identity_uuid(Provider::Claude, &id))
                .as_deref(),
            Some("uuid-same"),
            "identity must survive the re-sync"
        );
    }

    #[test]
    fn resolve_account_oauth_keeps_stored_tokens_when_a_different_account_is_live() {
        let home = TestHome::new("resolve-other");
        seed_account(&home.dir, "personal", "uuid-personal", "personal", 1_000);
        seed_live(&home.claude_home, "uuid-work", "work", 9_999_999_999_999);

        let resolved = resolve_account_oauth(Provider::Claude, "personal").expect("resolve should succeed");
        assert_eq!(
            token_of(&resolved, "accessToken"),
            "access-personal",
            "a different live account must never be adopted"
        );
        let stored = read_account_oauth(Provider::Claude, "personal").unwrap();
        assert_eq!(
            token_of(&stored, "accessToken"),
            "access-personal",
            "a different live account must never overwrite a stored one"
        );
    }

    #[test]
    fn resolve_account_oauth_falls_back_to_the_stored_copy_without_live_files() {
        let home = TestHome::new("resolve-nolive");
        seed_account(&home.dir, "work", "uuid-same", "stored", 1_000);
        // No live credentials/identity seeded at all.

        let resolved = resolve_account_oauth(Provider::Claude, "work").expect("resolve should succeed");
        assert_eq!(token_of(&resolved, "accessToken"), "access-stored");
    }

    // Switching away must first capture the outgoing account's rotated live
    // tokens, or switching back later re-installs a dead token and logs the user
    // out of Claude Code.
    #[test]
    fn switch_captures_the_outgoing_accounts_rotated_tokens_first() {
        let home = TestHome::new("switch-capture");
        seed_account(&home.dir, "work", "uuid-work", "work-stale", 1_000);
        seed_account(&home.dir, "personal", "uuid-personal", "personal", 9_999_999_999_999);
        seed_live(&home.claude_home, "uuid-work", "work-rotated", 9_999_999_999_999);

        switch(Provider::Claude, "personal").expect("switch should succeed");

        let outgoing = read_account_oauth(Provider::Claude, "work").unwrap();
        assert_eq!(
            token_of(&outgoing, "refreshToken"),
            "refresh-work-rotated",
            "the outgoing account's live tokens should have been captured"
        );
        let live = read_live_oauth(Provider::Claude).unwrap();
        assert_eq!(
            token_of(&live, "refreshToken"),
            "refresh-personal",
            "the incoming account's tokens should now be live"
        );
        let raw = fs::read_to_string(home.claude_home.join(".claude.json")).unwrap();
        let root: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            root.get("oauthAccount").and_then(|id| identity_uuid(Provider::Claude, &id)).as_deref(),
            Some("uuid-personal"),
            "the live identity should follow the tokens"
        );
        assert_eq!(
            root.get("projects").and_then(|pr| pr.get("keep")).and_then(|v| v.as_str()),
            Some("me"),
            "every other key in ~/.claude.json must be preserved"
        );
    }

    #[test]
    fn credman_flag_true_only_when_growthbook_feature_is_true() {
        assert!(credman_flag_from_claude_json(
            r#"{"cachedGrowthBookFeatures":{"tengu_windows_credman":true}}"#
        ));
    }

    #[test]
    fn credman_flag_false_when_absent_false_or_malformed() {
        assert!(!credman_flag_from_claude_json("{}"));
        assert!(!credman_flag_from_claude_json(
            r#"{"cachedGrowthBookFeatures":{"tengu_windows_credman":false}}"#
        ));
        assert!(!credman_flag_from_claude_json(
            r#"{"cachedGrowthBookFeatures":{}}"#
        ));
        assert!(!credman_flag_from_claude_json("not json"));
    }

    #[test]
    fn clear_accounts_removes_accounts_config_and_usage_but_keeps_window_state() {
        let home = TestHome::new("clear-accounts");
        seed_fake_account(&home.dir, "test1");
        fs::write(home.dir.join("window.json"), r#"{"x":0,"y":0}"#).unwrap();

        clear_accounts().expect("clear_accounts should succeed");

        assert!(!home.dir.join("accounts").exists(), "accounts/ should be removed");
        assert!(!home.dir.join("usage").exists(), "usage/ should be removed");
        assert!(!home.dir.join("config.json").exists(), "config.json should be removed");
        assert!(
            home.dir.join("window.json").exists(),
            "window.json must survive an 'accounts' clear"
        );
    }

    #[test]
    fn reset_all_removes_the_entire_base_dir() {
        let home = TestHome::new("reset-all");
        seed_fake_account(&home.dir, "test1");
        fs::write(home.dir.join("window.json"), r#"{"x":0,"y":0}"#).unwrap();

        reset_all().expect("reset_all should succeed");

        assert!(!home.dir.exists(), "a full reset should remove the entire base dir");
    }

    #[test]
    fn clear_accounts_and_reset_all_never_touch_a_sibling_claude_dir() {
        // Stands in for the real ~/.claude/ sitting next to ~/.switch-nextup/:
        // both destructive scopes must only ever remove things under base_dir().
        let home = TestHome::new("safety-boundary");
        seed_fake_account(&home.dir, "test1");
        let sibling_claude = home
            .dir
            .parent()
            .unwrap()
            .join(format!("claude-test-sibling-{}", home.dir.file_name().unwrap().to_str().unwrap()));
        fs::create_dir_all(&sibling_claude).unwrap();
        fs::write(sibling_claude.join(".credentials.json"), r#"{"claudeAiOauth":{}}"#).unwrap();

        clear_accounts().unwrap();
        assert!(
            sibling_claude.join(".credentials.json").exists(),
            "clear_accounts must never touch files outside base_dir()"
        );

        seed_fake_account(&home.dir, "test1"); // reset_all needs the dir to exist
        reset_all().unwrap();
        assert!(
            sibling_claude.join(".credentials.json").exists(),
            "reset_all must never touch files outside base_dir()"
        );

        let _ = fs::remove_dir_all(&sibling_claude);
    }
    // ----- Multi-provider behaviour -----

    #[test]
    fn legacy_flat_account_files_are_read_as_claude_and_migrate_on_write() {
        let home = TestHome::new("legacy-flat");
        // Written by a pre-multi-provider build: loose in accounts/, no subdir.
        seed_fake_account(&home.dir, "work");
        let flat = home.dir.join("accounts").join("work.json");
        assert!(flat.exists(), "seed should be in the legacy location");

        // Still visible, and attributed to Claude (the only provider back then).
        let listed = list_accounts_for(Provider::Claude).unwrap();
        assert!(
            listed.iter().any(|a| a.name == "work"),
            "a legacy flat account must not be orphaned by the upgrade"
        );
        assert!(list_accounts_for(Provider::Chatgpt).unwrap().is_empty());

        // Reading it works through the normal path too.
        let oauth = read_account_oauth(Provider::Claude, "work").unwrap();
        assert_eq!(oauth["accessToken"], "fake-access");

        // Any write migrates it into the provider subdirectory and drops the
        // loose original, so it cannot later show up twice.
        write_account_oauth(Provider::Claude, "work", &oauth).unwrap();
        assert!(
            home.dir.join("accounts").join("claude").join("work.json").exists(),
            "write should land in the provider subdirectory"
        );
        assert!(!flat.exists(), "the legacy file should be removed after migrating");
        assert_eq!(
            list_accounts_for(Provider::Claude).unwrap().len(),
            1,
            "the migrated account must not be listed twice"
        );
    }

    #[test]
    fn switching_chatgpt_leaves_the_live_claude_account_untouched() {
        let home = TestHome::new("provider-isolation");
        // A Claude account, currently live.
        seed_account(&home.dir, "work", "uuid-work", "claude-live", 9_999_999_999_999);
        seed_live(&home.claude_home, "uuid-work", "claude-live", 9_999_999_999_999);
        // Two ChatGPT accounts, the first of them live.
        seed_chatgpt_account(&home.dir, "cg-one", "acct-1", "one");
        seed_chatgpt_account(&home.dir, "cg-two", "acct-2", "two");
        seed_codex_live(&home.codex_home, "acct-1", "cg-one@example.com", "one");

        switch(Provider::Chatgpt, "cg-two").expect("chatgpt switch should succeed");

        // The Codex live file moved to the second account...
        assert_eq!(codex_live_token(&home.codex_home, "access_token"), "access-two");
        // ...and Claude's live credentials are byte-for-byte untouched. The two
        // clients read different files; one switch must never disturb the other.
        let claude_live = read_live_oauth(Provider::Claude).unwrap();
        assert_eq!(token_of(&claude_live, "accessToken"), "access-claude-live");
        assert_eq!(token_of(&claude_live, "refreshToken"), "refresh-claude-live");

        // Each provider has its own active pointer, set independently.
        let cfg = read_config();
        assert_eq!(cfg.active_for(Provider::Chatgpt).as_deref(), Some("cg-two"));
        assert_eq!(
            cfg.active_for(Provider::Claude).as_deref(),
            None,
            "a chatgpt switch must not invent or clear a claude pointer"
        );
    }

    #[test]
    fn chatgpt_switch_captures_the_outgoing_accounts_rotated_tokens_first() {
        let home = TestHome::new("chatgpt-rotation");
        // Codex rotated the live tokens behind our back (`last_refresh` moves),
        // so the stored copy for the outgoing account is stale.
        seed_chatgpt_account(&home.dir, "cg-one", "acct-1", "stale");
        seed_chatgpt_account(&home.dir, "cg-two", "acct-2", "two");
        seed_codex_live(&home.codex_home, "acct-1", "cg-one@example.com", "rotated");

        switch(Provider::Chatgpt, "cg-two").expect("switch should succeed");

        // The outgoing account must have kept the LIVE (rotated) tokens, or
        // switching back would reinstall a dead one and log the user out of Codex.
        let outgoing = read_account_oauth(Provider::Chatgpt, "cg-one").unwrap();
        assert_eq!(
            outgoing["tokens"]["refresh_token"], "refresh-rotated",
            "the rotated live token should have been folded into the stored copy"
        );

        // And switching back installs that live token, not the stale seed.
        switch(Provider::Chatgpt, "cg-one").expect("switch back should succeed");
        assert_eq!(codex_live_token(&home.codex_home, "refresh_token"), "refresh-rotated");
    }

    #[test]
    fn a_claude_and_a_chatgpt_account_may_share_a_name() {
        let home = TestHome::new("name-collision");
        seed_account(&home.dir, "work", "uuid-work", "claude", 9_999_999_999_999);
        seed_chatgpt_account(&home.dir, "work", "acct-1", "chatgpt");

        // Same name, different providers, different files — neither shadows the
        // other, which is the whole point of the per-provider subdirectories.
        let claude = read_account_oauth(Provider::Claude, "work").unwrap();
        let chatgpt = read_account_oauth(Provider::Chatgpt, "work").unwrap();
        assert_eq!(token_of(&claude, "accessToken"), "access-claude");
        assert_eq!(chatgpt["tokens"]["access_token"], "access-chatgpt");

        let all = list_all_accounts().unwrap();
        assert_eq!(all.len(), 2, "both should be listed");
        assert!(all.iter().any(|a| a.name == "work" && a.provider == "claude"));
        assert!(all.iter().any(|a| a.name == "work" && a.provider == "chatgpt"));

        // Removing one leaves the other alone.
        remove(Provider::Chatgpt, "work").unwrap();
        assert!(read_account_oauth(Provider::Claude, "work").is_ok());
        assert!(read_account_oauth(Provider::Chatgpt, "work").is_err());
    }

    #[test]
    fn chatgpt_accounts_report_plan_and_email_from_the_id_token() {
        let home = TestHome::new("chatgpt-info");
        seed_chatgpt_account(&home.dir, "cg", "acct-9", "tok");
        seed_codex_live(&home.codex_home, "acct-9", "cg@example.com", "tok");

        let listed = list_accounts_for(Provider::Chatgpt).unwrap();
        assert_eq!(listed.len(), 1);
        let acc = &listed[0];
        assert_eq!(acc.provider, "chatgpt");
        assert_eq!(acc.subscription_type.as_deref(), Some("business"));
        assert_eq!(acc.email.as_deref(), Some("cg@example.com"));
        assert!(acc.is_active, "the live codex account should be detected as active");
    }

    /// A disposable stand-in for the user's home directory, used by the
    /// data-dir migration tests. `resolve_base_dir` reads no env var and no
    /// global state, so these need neither the ENV_LOCK nor `TestHome`.
    struct FakeHome(PathBuf);

    impl FakeHome {
        fn new(label: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "switch-nextup-home-{label}-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, AtomicOrdering::Relaxed)
            ));
            fs::create_dir_all(&dir).unwrap();
            FakeHome(dir)
        }
    }

    impl Drop for FakeHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_pre_rename_data_dir_is_moved_across_on_first_run() {
        let home = FakeHome::new("migrate");
        let legacy = home.0.join(".claude-switch");
        fs::create_dir_all(legacy.join("accounts/claude")).unwrap();
        fs::write(legacy.join("accounts/claude/work.json"), "{}").unwrap();

        let resolved = resolve_base_dir(&home.0);

        assert_eq!(resolved, home.0.join(".switch-nextup"));
        assert!(
            resolved.join("accounts/claude/work.json").is_file(),
            "the saved account must survive the rename"
        );
        assert!(!legacy.exists(), "the old directory should be gone, not copied");
    }

    #[test]
    fn migration_never_overwrites_an_existing_new_data_dir() {
        let home = FakeHome::new("no-clobber");
        let legacy = home.0.join(".claude-switch");
        let target = home.0.join(".switch-nextup");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("stale.json"), "stale").unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("live.json"), "live").unwrap();

        let resolved = resolve_base_dir(&home.0);

        assert_eq!(resolved, target);
        assert_eq!(fs::read_to_string(target.join("live.json")).unwrap(), "live");
        assert!(!target.join("stale.json").exists(), "live data must win");
        assert!(legacy.join("stale.json").is_file(), "the legacy dir is left untouched");
    }

    #[test]
    fn a_fresh_install_just_gets_the_new_data_dir() {
        let home = FakeHome::new("fresh");
        assert_eq!(resolve_base_dir(&home.0), home.0.join(".switch-nextup"));
        assert!(!home.0.join(".switch-nextup").exists(), "resolving must not create it");
    }
}
