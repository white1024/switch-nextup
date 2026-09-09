mod login;
mod login_codex;
mod provider;
mod store;
mod usage;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use serde::Serialize;
use serde_json::Value;
use std::ffi::c_void;
use std::sync::Mutex;
use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem},
    tray::{TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, State, WebviewWindow,
};
use tauri_plugin_autostart::ManagerExt;

use crate::provider::Provider;

/// Holds a completed login's (provider, oauth, identity) between the login
/// command (when it can't yet tell which saved account this is, or that it's
/// brand new) and the frontend's follow-up `save_pending_login` once the user
/// has named it. The provider travels with it because a Claude and a ChatGPT
/// account may share a name — saving it under the wrong one would silently
/// overwrite the other provider's account.
#[derive(Default)]
struct PendingLoginState(Mutex<Option<(Provider, Value, Value)>>);

/// Holds the PKCE material for a login that has opened the browser and is
/// waiting for the user to paste the code back.
#[derive(Default)]
struct PendingAuthState(Mutex<Option<login::PendingAuth>>);

// Passed by the autostart entry so a login-triggered launch stays in the tray
// instead of popping the window. A manual launch has no such arg → window shows.
const AUTOSTART_ARG: &str = "--minimized";

// ----- Window backdrops -----
// Two subsystems are supported so we can compare them (dev):
//  * Modern DWM SystemBackdrop (Win11 22H2+): "acrylic" (transient), "mica".
//  * Legacy AccentPolicy (Win10+, undocumented user32 export): "legacy-acrylic"
//    (ACRYLICBLURBEHIND) and "blur" (BLURBEHIND — a plainer blur that does NOT
//    recede on focus loss, i.e. "permanent" frosted glass).
// When switching we disable the other subsystem first so they don't interfere.
#[cfg(windows)]
#[link(name = "dwmapi")]
extern "system" {
    fn DwmSetWindowAttribute(hwnd: isize, attr: u32, value: *const c_void, size: u32) -> i32;
}
// SetWindowCompositionAttribute is an undocumented user32 export that is NOT in
// user32.lib, so it cannot be statically linked (LNK2019/LNK1120). Resolve it at
// runtime via GetProcAddress instead. GetModuleHandleA/GetProcAddress live in
// kernel32, which is linked by default.
#[cfg(windows)]
type SetWindowCompositionAttributeFn =
    unsafe extern "system" fn(isize, *mut WindowCompositionAttribData) -> i32;

#[cfg(windows)]
extern "system" {
    fn GetModuleHandleA(name: *const u8) -> isize;
    fn GetProcAddress(module: isize, name: *const u8) -> *const c_void;
}

const DWMWA_USE_IMMERSIVE_DARK_MODE: u32 = 20;
const DWMWA_SYSTEMBACKDROP_TYPE: u32 = 38;
const DWMSBT_NONE: i32 = 1;
const DWMSBT_MAINWINDOW: i32 = 2; // Mica
const DWMSBT_TRANSIENTWINDOW: i32 = 3; // Acrylic

const WCA_ACCENT_POLICY: u32 = 19;
const ACCENT_DISABLED: u32 = 0;
const ACCENT_ENABLE_BLURBEHIND: u32 = 3;
const ACCENT_ENABLE_ACRYLICBLURBEHIND: u32 = 4;

#[cfg(windows)]
#[repr(C)]
struct AccentPolicy {
    accent_state: u32,
    accent_flags: u32,
    gradient_color: u32, // 0xAABBGGRR
    animation_id: u32,
}
#[cfg(windows)]
#[repr(C)]
struct WindowCompositionAttribData {
    attrib: u32,
    pv_data: *mut c_void,
    cb_data: usize,
}

fn hwnd_of(window: &WebviewWindow) -> Option<isize> {
    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

#[cfg(windows)]
fn set_dwm_backdrop(hwnd: isize, backdrop: i32) {
    let dark: i32 = 1;
    unsafe {
        DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const i32 as *const c_void, 4);
        DwmSetWindowAttribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, &backdrop as *const i32 as *const c_void, 4);
    }
}

#[cfg(windows)]
fn set_accent(hwnd: isize, state: u32, gradient_color: u32) {
    let mut policy = AccentPolicy {
        accent_state: state,
        accent_flags: 0,
        gradient_color,
        animation_id: 0,
    };
    let mut data = WindowCompositionAttribData {
        attrib: WCA_ACCENT_POLICY,
        pv_data: &mut policy as *mut _ as *mut c_void,
        cb_data: std::mem::size_of::<AccentPolicy>(),
    };
    unsafe {
        let module = GetModuleHandleA(b"user32.dll\0".as_ptr());
        if module == 0 {
            return;
        }
        let addr = GetProcAddress(module, b"SetWindowCompositionAttribute\0".as_ptr());
        if addr.is_null() {
            return;
        }
        let func: SetWindowCompositionAttributeFn = std::mem::transmute(addr);
        func(hwnd, &mut data as *mut _);
    }
}

// Warm brand tint (0xAABBGGRR) for the legacy blur/acrylic modes.
#[cfg(windows)]
fn brand_tint() -> u32 {
    let (r, g, b, a): (u32, u32, u32, u32) = (26, 23, 20, 0xCC);
    a << 24 | b << 16 | g << 8 | r
}

/// Apply the given window effect. One of "acrylic" | "mica" | "legacy-acrylic" |
/// "blur" | "none". Non-fatal on unsupported OS builds.
fn apply_effect(window: &WebviewWindow, effect: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        let hwnd = hwnd_of(window).ok_or("no Win32 window handle")?;
        match effect {
            "acrylic" => {
                set_accent(hwnd, ACCENT_DISABLED, 0);
                set_dwm_backdrop(hwnd, DWMSBT_TRANSIENTWINDOW);
            }
            "mica" => {
                set_accent(hwnd, ACCENT_DISABLED, 0);
                set_dwm_backdrop(hwnd, DWMSBT_MAINWINDOW);
            }
            "legacy-acrylic" => {
                set_dwm_backdrop(hwnd, DWMSBT_NONE);
                set_accent(hwnd, ACCENT_ENABLE_ACRYLICBLURBEHIND, brand_tint());
            }
            "blur" => {
                set_dwm_backdrop(hwnd, DWMSBT_NONE);
                set_accent(hwnd, ACCENT_ENABLE_BLURBEHIND, brand_tint());
            }
            "none" => {
                set_accent(hwnd, ACCENT_DISABLED, 0);
                set_dwm_backdrop(hwnd, DWMSBT_NONE);
            }
            other => return Err(format!("unknown window effect: {other}")),
        }
    }
    #[cfg(not(windows))]
    let _ = (window, effect);
    Ok(())
}

// ----- Tauri commands (invoked from the frontend) -----

#[tauri::command]
fn list_accounts() -> Result<Vec<store::AccountInfo>, String> {
    // Every provider in one call — each AccountInfo carries its `provider`
    // slug, and the frontend splits them into its tabs on that.
    store::list_all_accounts().map_err(|e| e.to_string())
}

/// Whether Windows Credential Manager sign-in mode looks active for Claude
/// Code, in which case file-swap switching may not take effect. See
/// `store::credman_guard_active`.
#[tauri::command]
fn check_credman_guard() -> bool {
    store::credman_guard_active()
}

#[tauri::command]
fn switch_account(app: AppHandle, provider: Provider, name: String) -> Result<(), String> {
    store::switch(provider, &name).map_err(|e| e.to_string())?;
    refresh_tray(&app);
    let _ = app.emit("accounts-changed", ());
    Ok(())
}

#[tauri::command]
fn add_current_account(app: AppHandle, provider: Provider, name: String) -> Result<(), String> {
    store::capture_current(provider, &name).map_err(|e| e.to_string())?;
    refresh_tray(&app);
    let _ = app.emit("accounts-changed", ());
    Ok(())
}

/// If the currently logged-in Claude account (matched by its stable identity,
/// not the OAuth tokens — those are fresh after a re-login) is already saved,
/// return its name. Lets the frontend update that account in place on "Save
/// current account" instead of asking the user to name/overwrite it again.
#[tauri::command]
fn find_current_account_match(provider: Provider) -> Option<String> {
    let identity = store::live_identity(provider)?;
    store::find_matching_account(provider, &identity)
}

#[derive(Serialize)]
// snake_case is NOT cosmetic: main.js branches on status === "saved" /
// "need_name". Without this serde emits the variant names verbatim ("Saved"),
// the match never fires, and a login that actually succeeded still asks the
// user to name an account it had already saved. Pinned by a test below.
#[serde(tag = "status", rename_all = "snake_case")]
enum LoginOutcome {
    Saved { name: String },
    NeedName { suggested: String },
}

/// If `identity`'s stable id matches the provider's current live identity, also
/// write the live files — this account IS the one that client is using, so its
/// own session benefits from the fresh tokens too. Otherwise never touch
/// `~/.claude/…` or `~/.codex/…`, mirroring the stable-id match rule
/// `store::list_accounts` already uses for active-detection.
fn sync_live_if_current(p: Provider, oauth: &Value, identity: &Value) {
    let live_id = store::live_identity(p).and_then(|id| provider::stable_id(p, &id));
    let this_id = provider::stable_id(p, identity);
    if this_id.is_some() && live_id == this_id {
        let _ = store::write_live_oauth(p, oauth);
        // Claude keeps its identity in a SECOND file; Codex carries it inside
        // the token, so there is nothing else to write for that provider.
        if p == Provider::Claude {
            let _ = store::write_live_oauth_account(identity);
        }
    }
}

/// Open the system browser on Anthropic's authorize page and hold the PKCE
/// material until the user pastes the code back. Returns the URL so the
/// frontend can show it if the browser did not come up.
#[tauri::command]
fn login_begin(auth: State<'_, PendingAuthState>) -> Result<String, String> {
    let (url, pending) = login::begin().map_err(|e| e.to_string())?;
    *auth.0.lock().unwrap() = Some(pending);
    Ok(url)
}

/// Finish the login from the code the user pasted. `name` is the explicit
/// target for a Re-login on a known-expired account; pass `None` for "Add
/// account", which tries to match the logged-in identity to an existing saved
/// account (e.g. re-authenticating one whose session expired differently)
/// before falling back to asking the frontend for a name.
#[tauri::command]
async fn login_submit_code(
    app: AppHandle,
    name: Option<String>,
    pasted: String,
    auth: State<'_, PendingAuthState>,
    pending: State<'_, PendingLoginState>,
) -> Result<LoginOutcome, String> {
    // Cloned, not taken: a rejected paste leaves the attempt usable so the user
    // can correct it without going back through the browser.
    let auth_pending = auth
        .0
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| "no login in progress — start Add account again".to_string())?;
    let (oauth, identity) = login::complete(&pasted, &auth_pending)
        .await
        .map_err(|e| e.to_string())?;
    *auth.0.lock().unwrap() = None;
    finish_login(&app, Provider::Claude, name, oauth, identity, &pending)
}

/// Run the in-app ChatGPT / Codex login. One step, not two: Codex's redirect
/// carries a fixed registered port, so the browser really does come back to us
/// and there is no code to paste (see `login_codex`). Blocks until the browser
/// round-trip finishes or times out.
#[tauri::command]
async fn login_codex_account(
    app: AppHandle,
    name: Option<String>,
    pending: State<'_, PendingLoginState>,
) -> Result<LoginOutcome, String> {
    let (blob, identity) = login_codex::login().await.map_err(|e| e.to_string())?;
    finish_login(&app, Provider::Chatgpt, name, blob, identity, &pending)
}

/// Save a completed login, or hand it back for naming when nothing matches.
/// Shared by both providers so the "which account is this?" rule cannot drift
/// between them.
fn finish_login(
    app: &AppHandle,
    p: Provider,
    name: Option<String>,
    oauth: Value,
    identity: Value,
    pending: &State<'_, PendingLoginState>,
) -> Result<LoginOutcome, String> {
    sync_live_if_current(p, &oauth, &identity);

    let target = match name {
        Some(n) => Some(n),
        None => store::find_matching_account(p, &identity),
    };

    if let Some(target) = target {
        store::write_login_result(p, &target, &oauth, &identity).map_err(|e| e.to_string())?;
        refresh_tray(app);
        let _ = app.emit("accounts-changed", ());
        return Ok(LoginOutcome::Saved { name: target });
    }

    let suggested = identity
        .get("emailAddress")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    *pending.0.lock().unwrap() = Some((p, oauth, identity));
    Ok(LoginOutcome::NeedName { suggested })
}

/// Finish an "Add account" login whose identity didn't match any saved
/// account, once the frontend has prompted the user for a name.
#[tauri::command]
fn save_pending_login(
    app: AppHandle,
    name: String,
    pending: State<'_, PendingLoginState>,
) -> Result<(), String> {
    let held = pending.0.lock().unwrap().take();
    let (p, oauth, identity) = held.ok_or("no pending login to save".to_string())?;
    store::write_login_result(p, &name, &oauth, &identity).map_err(|e| e.to_string())?;
    refresh_tray(&app);
    let _ = app.emit("accounts-changed", ());
    Ok(())
}

/// Discard a pending "Add account" login the user cancelled (closed the paste
/// box or the name prompt) instead of leaving its tokens held in memory.
#[tauri::command]
fn discard_pending_login(
    auth: State<'_, PendingAuthState>,
    pending: State<'_, PendingLoginState>,
) {
    *auth.0.lock().unwrap() = None;
    *pending.0.lock().unwrap() = None;
}

#[tauri::command]
fn remove_account(app: AppHandle, provider: Provider, name: String) -> Result<(), String> {
    store::remove(provider, &name).map_err(|e| e.to_string())?;
    refresh_tray(&app);
    let _ = app.emit("accounts-changed", ());
    Ok(())
}

#[tauri::command]
fn rename_account(app: AppHandle, provider: Provider, old: String, new: String) -> Result<(), String> {
    store::rename(provider, &old, &new).map_err(|e| e.to_string())?;
    refresh_tray(&app);
    let _ = app.emit("accounts-changed", ());
    Ok(())
}

#[tauri::command]
async fn get_usage(provider: Provider, name: String) -> Result<usage::Usage, String> {
    usage::fetch_usage_for(provider, &name)
        .await
        .map_err(|e| e.to_string())
}

/// Last-known usage from disk with no network call — for non-active accounts, so
/// they show numbers without hitting the rate-limited endpoint. None if never fetched.
#[tauri::command]
fn get_cached_usage(provider: Provider, name: String) -> Option<usage::Usage> {
    usage::cached_usage_for(provider, &name)
}

#[tauri::command]
fn set_window_effect(window: WebviewWindow, effect: String) -> Result<(), String> {
    apply_effect(&window, &effect)
}

/// Hide the window to the tray, saving its position first so a later relaunch of
/// the app restores it. The custom ✕ button calls this instead of a bare hide().
/// Clear this app's stored data. `scope` = "accounts" (saved accounts + active
/// pointer + usage cache) or "all" (the whole `~/.switch-nextup/` dir, and disable
/// autostart). **Never touches Claude Code's own credential files**, so it does not
/// log the user out of Claude Code. UI prefs (localStorage) are cleared frontend-side.
#[tauri::command]
fn clear_data(app: AppHandle, scope: String) -> Result<(), String> {
    match scope.as_str() {
        "accounts" => store::clear_accounts().map_err(|e| e.to_string())?,
        "all" => {
            store::reset_all().map_err(|e| e.to_string())?;
            let _ = app.autolaunch().disable(); // best-effort; fine if not registered
        }
        other => return Err(format!("unknown clear scope: {other}")),
    }
    refresh_tray(&app);
    let _ = app.emit("accounts-changed", ());
    Ok(())
}

#[tauri::command]
fn hide_to_tray(window: WebviewWindow) {
    if let Ok(p) = window.outer_position() {
        store::save_window_pos(p.x, p.y);
    }
    let _ = window.hide();
}

// Compact-mode window heights (width stays the configured 400px either way;
// only the height changes, so the window shrinks to a thin bar anchored at
// its current top-left position instead of moving). FULL_HEIGHT matches the
// height configured in tauri.conf.json. The compact bar holds one row per
// provider that has an active account (Claude and/or ChatGPT can both be
// active at once), so its height scales with `rows` rather than being fixed —
// COMPACT_TITLEBAR_HEIGHT + COMPACT_ROW_HEIGHT * 1 reproduces the old fixed
// 88px for the single-row case.
const FULL_HEIGHT: f64 = 450.0;
const COMPACT_TITLEBAR_HEIGHT: f64 = 42.0;
const COMPACT_ROW_HEIGHT: f64 = 46.0;

/// Resize the window between its normal size and a compact bar (titlebar +
/// `rows` usage strips, at least 1). The frontend swaps the content that
/// fills that space; this just makes the window itself match.
#[tauri::command]
fn set_compact(window: WebviewWindow, compact: bool, rows: u32) -> Result<(), String> {
    let height = if compact {
        COMPACT_TITLEBAR_HEIGHT + COMPACT_ROW_HEIGHT * rows.max(1) as f64
    } else {
        FULL_HEIGHT
    };
    window
        .set_size(tauri::LogicalSize::new(400.0, height))
        .map_err(|e| e.to_string())
}

// ----- System tray -----

fn build_tray_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let menu = Menu::new(app)?;

    // One labelled section per provider. A tray menu cannot have tabs like the
    // window does, and a flat list would show two ● markers at once (each
    // provider has its OWN active account), which reads as a bug. A disabled
    // header item plus a separator is the closest tray equivalent.
    let mut wrote_any = false;
    for p in Provider::ALL {
        let accounts = store::list_accounts_for(p).unwrap_or_default();
        if accounts.is_empty() {
            continue;
        }
        if wrote_any {
            menu.append(&PredefinedMenuItem::separator(app)?)?;
        }
        let header = MenuItem::with_id(
            app,
            format!("header::{}", p.slug()),
            p.label(),
            false,
            None::<&str>,
        )?;
        menu.append(&header)?;
        for acc in &accounts {
            let mark = if acc.is_active { "● " } else { "○ " };
            let label = format!("{mark}{}", acc.name);
            let item = MenuItem::with_id(
                app,
                // Provider-qualified so a Claude and a ChatGPT account sharing
                // a name cannot collide on the same menu id.
                format!("switch::{}::{}", p.slug(), acc.name),
                label,
                !acc.is_active,
                None::<&str>,
            )?;
            menu.append(&item)?;
        }
        wrote_any = true;
    }

    if !wrote_any {
        let empty = MenuItem::with_id(app, "noop", "No accounts saved yet", false, None::<&str>)?;
        menu.append(&empty)?;
    }

    menu.append(&PredefinedMenuItem::separator(app)?)?;
    let show = MenuItem::with_id(app, "show", "Open Switch NextUp", true, None::<&str>)?;
    // "Start with Windows" (run-at-login). Checked = registered in the OS
    // autostart. Toggled in handle_menu_event via the autostart plugin.
    let autostart_on = app.autolaunch().is_enabled().unwrap_or(false);
    let autostart = CheckMenuItem::with_id(
        app,
        "toggle-autostart",
        "Start with Windows",
        true,
        autostart_on,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    menu.append(&show)?;
    menu.append(&autostart)?;
    menu.append(&quit)?;
    Ok(menu)
}

fn refresh_tray(app: &AppHandle) {
    if let Some(tray) = app.tray_by_id("main") {
        if let Ok(menu) = build_tray_menu(app) {
            let _ = tray.set_menu(Some(menu));
        }
    }
}

fn show_main_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}

fn handle_menu_event(app: &AppHandle, id: &str) {
    match id {
        "quit" => {
            // Save position before exit so a relaunch restores it.
            if let Some(win) = app.get_webview_window("main") {
                if let Ok(p) = win.outer_position() {
                    store::save_window_pos(p.x, p.y);
                }
            }
            app.exit(0)
        }
        "show" => show_main_window(app),
        "toggle-autostart" => {
            let mgr = app.autolaunch();
            let enabled = mgr.is_enabled().unwrap_or(false);
            let _ = if enabled { mgr.disable() } else { mgr.enable() };
            refresh_tray(app); // reflect the new check state
        }
        other => {
            // "switch::<provider slug>::<account name>". An account name may
            // itself contain "::"-ish text, so split off only the provider and
            // keep the rest of the string as the name.
            if let Some(rest) = other.strip_prefix("switch::") {
                if let Some((slug, name)) = rest.split_once("::") {
                    if let Some(p) = Provider::from_slug(slug) {
                        if store::switch(p, name).is_ok() {
                            refresh_tray(app);
                            let _ = app.emit("accounts-changed", ());
                        }
                    }
                }
            }
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default();

    // Without this, launching the .exe again while a copy is already running
    // just starts a second full process — two tray icons, two windows, and two
    // instances independently overwriting the same ~/.switch-nextup/ files
    // (window position, account store, usage cache). Must be registered before
    // other plugins/setup so a second launch hands off and exits as early as
    // possible. Desktop-only per the plugin's own design (mobile has no concept
    // of "launch again while running").
    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
        show_main_window(app);
    }));

    builder
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec![AUTOSTART_ARG]),
        ))
        .manage(PendingLoginState::default())
        .manage(PendingAuthState::default())
        .invoke_handler(tauri::generate_handler![
            list_accounts,
            check_credman_guard,
            switch_account,
            add_current_account,
            find_current_account_match,
            login_begin,
            login_submit_code,
            login_codex_account,
            save_pending_login,
            discard_pending_login,
            remove_account,
            rename_account,
            get_usage,
            get_cached_usage,
            set_window_effect,
            clear_data,
            hide_to_tray,
            set_compact,
        ])
        .setup(|app| {
            let handle = app.handle().clone();

            // Restore the last window position, then apply the default
            // frosted-glass effect and show. The window starts hidden
            // (`visible:false` in tauri.conf.json) so it appears already at the
            // restored spot instead of flashing at the default position first.
            // Focus it too: acrylic renders washed-out while inactive, and when
            // launched from a terminal (`npm run dev`) it isn't the foreground
            // window by default. The frontend re-applies the user's saved backdrop
            // choice (and re-asserts it on focus) once loaded.
            // A login-triggered autostart launch passes AUTOSTART_ARG — stay in
            // the tray (don't pop the window). A manual launch shows it normally.
            let start_hidden = std::env::args().any(|a| a == AUTOSTART_ARG);
            if let Some(win) = app.get_webview_window("main") {
                if let Some(pos) = store::read_window_pos() {
                    let _ = win.set_position(tauri::PhysicalPosition::new(pos.x, pos.y));
                }
                let _ = apply_effect(&win, "acrylic");
                if !start_hidden {
                    let _ = win.show();
                    let _ = win.set_focus();
                }
            }

            let menu = build_tray_menu(&handle)?;
            TrayIconBuilder::with_id("main")
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("Switch NextUp")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| handle_menu_event(app, event.id.as_ref()))
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click { button, .. } = event {
                        if matches!(button, tauri::tray::MouseButton::Left) {
                            show_main_window(tray.app_handle());
                        }
                    }
                })
                .build(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            // Keep running in the tray when the window is closed; save position
            // first so a relaunch restores it. (The custom ✕ goes through the
            // hide_to_tray command, which also saves; this covers any other close.)
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if let Ok(p) = window.outer_position() {
                    store::save_window_pos(p.x, p.y);
                }
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running Switch NextUp");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the IPC wire format against main.js, which branches on
    /// `status === "saved"` / `"need_name"`. serde defaults to the variant
    /// names ("Saved"/"NeedName"); with that default the frontend match never
    /// fires and a login that already saved the account still prompts for a
    /// name. cargo test and node --check are both green while that is broken,
    /// so the contract has to be asserted here.
    #[test]
    fn login_outcome_serializes_the_status_tags_main_js_matches_on() {
        let saved = serde_json::to_value(LoginOutcome::Saved {
            name: "work".into(),
        })
        .unwrap();
        assert_eq!(saved["status"], "saved");
        assert_eq!(saved["name"], "work");

        let need = serde_json::to_value(LoginOutcome::NeedName {
            suggested: "a@b.c".into(),
        })
        .unwrap();
        assert_eq!(need["status"], "need_name");
        assert_eq!(need["suggested"], "a@b.c");
    }
}
