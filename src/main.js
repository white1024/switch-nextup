const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const appWindow = window.__TAURI__.window.getCurrentWindow();

const accountsEl = document.getElementById("accounts");
const errorEl = document.getElementById("error");
const errorMsgEl = document.getElementById("error-msg");
const credmanWarningEl = document.getElementById("credman-warning");
const refreshBtn = document.getElementById("refresh-btn");
const addBtn = document.getElementById("add-btn");
const loginBtn = document.getElementById("login-btn");

let accounts = [];
let credmanGuardActive = false;
// cacheKey(provider, name) -> { state: "loading"|"ok"|"error"|"ratelimited"|"cached",
//           data|message, fetchedAt, retryAttempt, retryIn, retryTimer }
// Keyed by provider+name, not name alone — a Claude and a ChatGPT account may
// legitimately share a name, and both providers are now live-fetched.
const usageCache = {};
function cacheKey(provider, name) {
  return `${provider}::${name}`;
}
// Don't refetch a fresh, successful usage result within this window. The
// /api/oauth/usage endpoint is low-rate-limit (meant for occasional /usage
// calls), so switching/renaming/hot-reload should reuse cached bars.
const USAGE_TTL_MS = 60_000;
const RATE_LIMIT_MAX_RETRIES = 3;
// Auto-refresh the active account's usage this often while the window is visible.
// Kept well above USAGE_TTL_MS so each tick is mostly a no-op (TTL-gated); usage
// %s don't change second-to-second, and the endpoint is rate-limited.
const AUTO_REFRESH_MS = 120_000;

// ---------- providers ----------
// Every backend account command is provider-scoped, and the two providers are
// independent: one active Claude account AND one active ChatGPT account exist at
// the same time. A Claude and a ChatGPT account may also share a name, so a
// name-collision check must compare the provider too.
const PROVIDER_CLAUDE = "claude";
const PROVIDER_CHATGPT = "chatgpt";

/// The provider the footer's "Save current account" acts on — i.e. the selected
/// tab.
function currentProvider() {
  return currentTab;
}

function sameProvider(acc, provider) {
  return (acc.provider || PROVIDER_CLAUDE) === provider;
}

// ---------- provider tabs (Claude / ChatGPT) ----------
// A tray-menu-style single active-per-provider list wouldn't fit this widget's
// glanceability (the user picked tabs over grouped/flat layouts after seeing
// mockups — see the recorded decision; do not revert to a single list).
const tabsEl = document.getElementById("tabs");
let currentTab = localStorage.getItem("activeTab") === PROVIDER_CHATGPT ? PROVIDER_CHATGPT : PROVIDER_CLAUDE;

function updateFooterForTab() {
  const isClaude = currentTab === PROVIDER_CLAUDE;
  addBtn.title = isClaude
    ? "Capture the account currently logged into Claude Code"
    : "Capture the account currently logged into ChatGPT / Codex";
  // Both providers have an in-app login now; only the shape differs (Claude
  // pastes a code, ChatGPT round-trips through a loopback port).
  loginBtn.title = isClaude
    ? "Log into a Claude account in your browser, right from here"
    : "Log into a ChatGPT / Codex account in your browser, right from here";
  loginBtn.classList.remove("hidden");
}

function setTab(tab) {
  currentTab = tab;
  localStorage.setItem("activeTab", tab);
  for (const b of tabsEl.querySelectorAll(".tab-btn")) {
    b.classList.toggle("active", b.dataset.provider === tab);
  }
  updateFooterForTab();
  render();
}

tabsEl.addEventListener("click", (ev) => {
  const btn = ev.target.closest(".tab-btn");
  if (!btn) return;
  setTab(btn.dataset.provider);
});

(function initTabs() {
  for (const b of tabsEl.querySelectorAll(".tab-btn")) {
    b.classList.toggle("active", b.dataset.provider === currentTab);
  }
  updateFooterForTab();
})();

// ---------- in-app modal (themed replacement for prompt/confirm) ----------
const modalOverlay = document.getElementById("modal-overlay");
const modalTitle = document.getElementById("modal-title");
const modalInput = document.getElementById("modal-input");
const modalMessage = document.getElementById("modal-message");
const modalChoices = document.getElementById("modal-choices");
const modalOk = document.getElementById("modal-ok");
const modalCancel = document.getElementById("modal-cancel");
let modalMode = null; // "prompt" | "confirm" | "choice"
let modalResolve = null;

function closeModal(result) {
  modalOverlay.classList.add("hidden");
  // Reset choice-mode extras so the next prompt/confirm renders normally.
  modalChoices.classList.add("hidden");
  modalChoices.innerHTML = "";
  modalOk.classList.remove("hidden");
  const resolve = modalResolve;
  modalResolve = null;
  modalMode = null;
  if (resolve) resolve(result);
}

// Resolves to the entered string, or null if cancelled.
function openPrompt({ title, message = "", value = "", placeholder = "", okLabel = "OK" }) {
  return new Promise((resolve) => {
    modalResolve = resolve;
    modalMode = "prompt";
    modalTitle.textContent = title;
    modalInput.classList.remove("hidden");
    modalInput.value = value;
    modalInput.placeholder = placeholder;
    modalMessage.classList.toggle("hidden", !message);
    modalMessage.textContent = message;
    modalOk.textContent = okLabel;
    modalOverlay.classList.remove("hidden");
    setTimeout(() => {
      modalInput.focus();
      modalInput.select();
    }, 0);
  });
}

// Resolves to true (confirmed) or false (cancelled).
function openConfirm({ title, message, okLabel = "OK" }) {
  return new Promise((resolve) => {
    modalResolve = resolve;
    modalMode = "confirm";
    modalTitle.textContent = title;
    modalInput.classList.add("hidden");
    modalInput.value = "";
    modalMessage.classList.remove("hidden");
    modalMessage.textContent = message;
    modalOk.textContent = okLabel;
    modalOverlay.classList.remove("hidden");
    setTimeout(() => modalOk.focus(), 0);
  });
}

// Resolves to the picked choice `value`, or null if cancelled. Each choice is
// { label, value, danger? } and renders as its own button (the choices ARE the
// actions, so the default OK button is hidden).
function openChoice({ title, message, choices }) {
  return new Promise((resolve) => {
    modalResolve = resolve;
    modalMode = "choice";
    modalTitle.textContent = title;
    modalInput.classList.add("hidden");
    modalInput.value = "";
    modalMessage.classList.remove("hidden");
    modalMessage.textContent = message;
    modalOk.classList.add("hidden");
    modalChoices.innerHTML = "";
    for (const c of choices) {
      const b = document.createElement("button");
      b.className = "modal-btn choice" + (c.danger ? " danger" : "");
      b.textContent = c.label;
      b.addEventListener("click", () => closeModal(c.value));
      modalChoices.appendChild(b);
    }
    modalChoices.classList.remove("hidden");
    modalOverlay.classList.remove("hidden");
    setTimeout(() => modalCancel.focus(), 0);
  });
}

modalOk.addEventListener("click", () =>
  closeModal(modalMode === "confirm" ? true : modalInput.value)
);
modalCancel.addEventListener("click", () =>
  closeModal(modalMode === "confirm" ? false : null)
);
modalOverlay.addEventListener("mousedown", (ev) => {
  if (ev.target === modalOverlay) closeModal(modalMode === "confirm" ? false : null);
});
modalInput.addEventListener("keydown", (ev) => {
  if (ev.key === "Enter") closeModal(modalInput.value);
});
document.addEventListener("keydown", (ev) => {
  if (modalOverlay.classList.contains("hidden")) return;
  if (ev.key === "Escape") closeModal(modalMode === "confirm" ? false : null);
});

// ---------- frameless window controls ----------
// The single ✕ button hides the window to the tray (does not quit). Routed
// through the backend so it saves the window position before hiding, so a later
// relaunch of the app reopens at the same spot.
document.getElementById("close-btn").addEventListener("click", () => invoke("hide_to_tray"));

// ---------- always-on-top (pin) ----------
const pinBtn = document.getElementById("pin-btn");
let pinned = localStorage.getItem("pinned") === "true";

async function applyPin(on) {
  pinned = on;
  localStorage.setItem("pinned", String(on));
  pinBtn.classList.toggle("active", on);
  pinBtn.title = on ? "Unpin (stop staying on top)" : "Keep on top";
  try {
    await appWindow.setAlwaysOnTop(on);
  } catch (e) {
    console.warn("setAlwaysOnTop failed:", e);
  }
}

pinBtn.addEventListener("click", () => applyPin(!pinned));
applyPin(pinned); // restore saved pin state on load

// ---------- backdrop switching (DWM vs legacy, for dev comparison) ----------
const EFFECTS = ["acrylic", "mica", "legacy-acrylic", "blur", "none"];
const EFFECT_LABELS = {
  acrylic: "Acrylic · DWM",
  mica: "Mica · DWM",
  "legacy-acrylic": "Acrylic · legacy",
  blur: "Blur · legacy",
  none: "None",
};
const effectLabel = document.getElementById("effect-label"); // DEV
let effectIdx = 0;

async function applyEffect(effect) {
  document.body.dataset.effect = effect;
  localStorage.setItem("effect", effect);
  if (effectLabel) effectLabel.textContent = EFFECT_LABELS[effect] || effect; // DEV
  try {
    await invoke("set_window_effect", { effect });
  } catch (e) {
    // Non-fatal (e.g. effect unsupported on this OS build).
    console.warn("set_window_effect failed:", e);
  }
}

document.getElementById("effect-btn").addEventListener("click", () => {
  effectIdx = (effectIdx + 1) % EFFECTS.length;
  applyEffect(EFFECTS[effectIdx]);
});

(function initEffect() {
  const saved = localStorage.getItem("effect");
  const idx = EFFECTS.indexOf(saved);
  effectIdx = idx >= 0 ? idx : 0;
  applyEffect(EFFECTS[effectIdx]);
})();

// ---------- compact mode: shrink to a titlebar + one usage strip ----------
const compactBtn = document.getElementById("compact-btn");
const compactUsageEl = document.getElementById("compact-usage");
let compact = localStorage.getItem("compact") === "true";

async function applyCompact(on) {
  compact = on;
  localStorage.setItem("compact", String(on));
  document.body.dataset.compact = String(on);
  compactBtn.classList.toggle("active", on);
  compactBtn.title = on ? "Expand full view" : "Collapse to compact bar";
  compactUsageEl.classList.toggle("hidden", !on);
  // Force the next resize to actually fire even if the row count matches
  // whatever it was the last time compact mode was on.
  lastCompactRows = null;
  if (on) {
    updateCompactUsage(); // renders the rows AND resizes to fit them
    return;
  }
  try {
    await invoke("set_compact", { compact: false, rows: 1 });
  } catch (e) {
    // Non-fatal — the CSS layout still switches even if the native resize fails.
    console.warn("set_compact failed:", e);
  }
}

compactBtn.addEventListener("click", () => applyCompact(!compact));
applyCompact(compact); // restore saved state on load

const toastEl = document.getElementById("toast");
let toastTimer = null;
let toastFadeTimer = null;

function hideToast() {
  if (toastTimer) clearTimeout(toastTimer);
  if (toastFadeTimer) clearTimeout(toastFadeTimer);
  toastEl.classList.add("fade");
  toastFadeTimer = setTimeout(() => toastEl.classList.add("hidden"), 200);
}

// Show a message. `html` is trusted markup built by callers (only escaped
// account names are interpolated). Auto-hides after opts.ms (default 6s).
function showToast(html, opts = {}) {
  if (toastTimer) clearTimeout(toastTimer);
  if (toastFadeTimer) clearTimeout(toastFadeTimer);
  const { ms = 6000 } = opts;
  toastEl.innerHTML = `<div class="toast-msg">${html}</div>`;
  const row = document.createElement("div");
  row.className = "toast-actions";
  const dismiss = document.createElement("button");
  dismiss.className = "toast-btn";
  dismiss.textContent = "Dismiss";
  dismiss.addEventListener("click", hideToast);
  row.appendChild(dismiss);
  toastEl.appendChild(row);
  toastEl.classList.remove("hidden", "fade");
  toastTimer = setTimeout(hideToast, ms);
}

function showError(msg) {
  if (!msg) {
    errorEl.classList.add("hidden");
    errorMsgEl.textContent = "";
  } else {
    errorEl.classList.remove("hidden");
    errorMsgEl.textContent = msg;
  }
}
document.getElementById("error-dismiss").addEventListener("click", () => showError(""));

function barColor(pct) {
  if (pct >= 90) return "var(--red)";
  if (pct >= 70) return "var(--yellow)";
  return "var(--green)";
}

// A rate-limit window whose reset time is this far past has rolled over, and any
// percentage we cached for it belongs to the *previous* window. The grace period
// keeps a genuine "resetting right now" reading (taken seconds after a reset, or
// with a little clock skew) reading as such.
const RESET_GRACE_MS = 5 * 60 * 1000;

// True when a bucket's window has since rolled over, so its cached number no
// longer describes anything. Only rate-limit buckets roll over — credit and
// spend buckets carry a fixed expiry date (or none), and a `limit_dollars`
// figure is what tells them apart (same test as `compactLabelText`).
function windowRolledOver(b) {
  if (!b || !b.resets_at || b.limit_dollars != null) return false;
  const t = new Date(b.resets_at);
  return !isNaN(t) && Date.now() - t > RESET_GRACE_MS;
}

function fmtReset(iso) {
  if (!iso) return "";
  const d = new Date(iso);
  if (isNaN(d)) return "";
  const diffMs = d - new Date();
  // Anything further past than RESET_GRACE_MS never reaches here — those buckets
  // are dropped by `windowRolledOver` rather than described as resetting.
  if (diffMs <= 0) return "resets now";
  const mins = Math.round(diffMs / 60000);
  if (mins < 60) return `resets in ${mins}m`;
  const hrs = Math.floor(mins / 60);
  const rem = mins % 60;
  if (hrs < 24) return `resets in ${hrs}h ${rem}m`;
  const days = Math.floor(hrs / 24);
  return `resets in ${days}d ${hrs % 24}h`;
}

function fmtDate(iso) {
  const d = new Date(iso);
  if (isNaN(d)) return "";
  return d.toLocaleDateString("en-US", { year: "numeric", month: "long", day: "numeric" });
}

// Relative age of a unix-ms timestamp: "just now", "5m ago", "3h ago", "2d ago".
function fmtAgo(ms) {
  if (!ms) return "";
  const mins = Math.round((Date.now() - ms) / 60000);
  if (mins < 1) return "just now";
  if (mins < 60) return `${mins}m ago`;
  const hrs = Math.floor(mins / 60);
  if (hrs < 24) return `${hrs}h ago`;
  return `${Math.floor(hrs / 24)}d ago`;
}

// Always two decimals, like the official usage panel ("$0.00 of $150.00"). Without
// a minimum, round figures lost them inconsistently — $986.49534 rendered "$986.5",
// which reads as a glitch rather than money.
function fmtDollars(n) {
  if (n == null) return "";
  return (
    "$" +
    Number(n).toLocaleString("en-US", { minimumFractionDigits: 2, maximumFractionDigits: 2 })
  );
}

function meterHtml(b) {
  if (!b) return "";
  // Its window rolled over since this reading, so the number would be a stale
  // claim about a window that no longer exists. Callers drop the empty string.
  if (windowRolledOver(b)) return "";
  const pct = Math.max(0, Math.min(100, b.utilization));
  const detail = [];
  if (b.subtext) detail.push(b.subtext); // e.g. "One-time credit"
  if (b.limit_dollars != null) {
    detail.push(`${fmtDollars(b.used_dollars ?? 0)} of ${fmtDollars(b.limit_dollars)}`);
  }
  if (b.resets_at) {
    // Credit buckets "expire" (absolute date); rate-limit buckets "reset" (relative).
    detail.push(b.subtext ? `Expires ${fmtDate(b.resets_at)}` : fmtReset(b.resets_at));
  }
  const sub = detail.filter(Boolean).join(" · ");
  return `
    <div class="meter">
      <div class="meter-label">
        <span>${escapeHtml(b.label)}${sub ? ` · ${sub}` : ""}</span>
        <span class="pct">${pct.toFixed(0)}%</span>
      </div>
      <div class="bar"><div class="fill" style="width:${pct}%;background:${barColor(pct)}"></div></div>
    </div>`;
}

// `acc` (not just a name) so this can gate the Claude-only Re-login button and
// look up the right cache entry — a Claude and a ChatGPT account may share a
// name, so the cache is keyed by provider+name (see cacheKey).
function usageHtml(acc) {
  const name = acc.name;
  const provider = acc.provider || PROVIDER_CLAUDE;
  const isClaude = provider === PROVIDER_CLAUDE;
  const u = usageCache[cacheKey(provider, name)];
  if (!u || u.state === "loading") {
    return `<div class="usage-note">Loading usage…</div>`;
  }
  if (u.state === "error") {
    return `<div class="usage-note err">Usage unavailable: ${u.message}</div>`;
  }
  if (u.state === "ratelimited") {
    const note = u.retryIn
      ? `Rate limited — retrying in ${Math.round(u.retryIn / 1000)}s…`
      : `Rate limited — click ⟳ to retry.`;
    return `<div class="usage-note err">${note}</div>`;
  }
  // Non-active accounts: last-known numbers only, rendered quietly (no red note,
  // no retry). Refreshed whenever this account was last the active one.
  if (u.state === "cached") {
    if (!u.data) {
      return `<div class="usage-note">Switch to this account to load its usage.</div>`;
    }
    const cached = u.data.buckets || [];
    const meters = cached.map(meterHtml).filter(Boolean).join("");
    const age = fmtAgo(u.data.fetched_at);
    const note = age ? `<div class="usage-note">As of ${age}</div>` : "";
    const creditsNote = u.data.credits_note
      ? `<div class="usage-note">${escapeHtml(u.data.credits_note)}</div>`
      : "";
    if (!meters) {
      // Distinguish "we have a reading, but every window in it has since rolled
      // over" from "we never recorded anything" — only the former is worth a
      // switch to refresh.
      if (cached.length) {
        return `<div class="usage-note">Last reading${age ? ` ${age}` : ""} has since reset —
          switch to this account for current usage.</div>`;
      }
      if (creditsNote) return `${creditsNote}${note}`;
      return `<div class="usage-note">No usage recorded yet.</div>`;
    }
    return `<div class="meters">${meters}</div>${creditsNote}${note}`;
  }
  const d = u.data;
  // When a live fetch failed, the backend returns last-known data flagged stale.
  let staleNote = "";
  if (d.stale) {
    const age = fmtAgo(d.fetched_at);
    const expired = /session.?expired/i.test(d.note || "");
    if (expired && isClaude) {
      // In-app Re-login only drives Claude Code's OAuth flow so far.
      staleNote = `<div class="usage-note err">Session expired.
        <button class="relogin-btn" data-act="relogin" data-name="${escapeAttr(name)}">Re-login</button>
      </div>`;
    } else if (expired) {
      staleNote = `<div class="usage-note err">${escapeHtml(d.note)}</div>`;
    } else {
      const rl = /rate.?limit|\b429\b/i.test(d.note || "");
      const why = rl ? "rate limited" : "couldn’t refresh";
      staleNote = `<div class="usage-note err">Last known${age ? ` · ${age}` : ""} · ${why}, retrying…</div>`;
    }
  }
  const meters = (d.buckets || []).map(meterHtml).filter(Boolean).join("");
  const creditsNote = d.credits_note
    ? `<div class="usage-note">${escapeHtml(d.credits_note)}</div>`
    : "";
  if (!meters) {
    return staleNote || creditsNote || `<div class="usage-note">No active limits reported.</div>`;
  }
  // `extra_usage` reports the SAME monthly figure as the `spend` bucket
  // (extra_usage.monthly_limit is spend.limit), so once that meter renders this
  // note is the same percentage twice — with less detail. Keep it only as a
  // fallback for a plan that reports the utilization but no spend limit. Claude-only:
  // the backend never sets extra_usage_enabled for ChatGPT.
  let extra = "";
  const hasSpendMeter = (d.buckets || []).some((b) => b.key === "spend");
  if (!hasSpendMeter && d.extra_usage_enabled && d.extra_usage_utilization != null) {
    extra = `<div class="usage-note">Extra usage: ${d.extra_usage_utilization.toFixed(0)}% of monthly credits</div>`;
  }
  return `<div class="meters">${meters}</div>${extra}${creditsNote}${staleNote}`;
}

// ---------- compact-mode usage strip (active account only) ----------
// Full bucket labels are sentences ("Current week (Sonnet only)", or for
// credit-style plans "Claude Code and Cowork credit") — too long for a thin
// strip, and mid-word CSS ellipsis just mangles them. Use the raw bucket `key`
// (stable, unlike the friendly label) to show short forms for the known
// buckets, and a generic "Credit" tag for credit-style ones instead.
const COMPACT_LABELS = {
  five_hour: "5h",
  seven_day: "7d",
  seven_day_sonnet: "7d Sonnet",
  seven_day_opus: "7d Opus",
  // The plan's real money limit. Carries limit_dollars, so without an entry here
  // it would fall through to the generic "Credit" tag and read as a promo credit.
  spend: "Spend",
};
function compactLabelText(b) {
  if (COMPACT_LABELS[b.key]) return COMPACT_LABELS[b.key];
  if (b.limit_dollars != null) return "Credit";
  return b.label; // unknown, non-credit bucket — rare; shown in full, can scroll
}

function compactMeterHtml(b) {
  if (!b) return "";
  if (windowRolledOver(b)) return ""; // stale window — same rule as meterHtml
  const pct = Math.max(0, Math.min(100, b.utilization));
  return `
    <span class="compact-meter" title="${escapeAttr(b.label)}: ${pct.toFixed(0)}%">
      <span class="compact-label">${escapeHtml(compactLabelText(b))}</span>
      <span class="compact-bar"><span class="fill" style="width:${pct}%;background:${barColor(pct)}"></span></span>
      <span class="compact-pct">${pct.toFixed(0)}%</span>
    </span>`;
}

// Both providers can have an active account at the same time, so the strip is
// one ROW per provider that currently has one — not just Claude. Each row is
// tagged with its provider name (`.compact-provider`) so a Claude account and
// a ChatGPT account that happen to share a name are never ambiguous, even
// though row position alone (e.g. only one provider active) would not be a
// reliable enough cue on its own.
const PROVIDER_LABELS = { [PROVIDER_CLAUDE]: "Claude", [PROVIDER_CHATGPT]: "ChatGPT" };

// One provider's row, mirroring usageHtml's states condensed to fit a single
// line. Returns null when that provider has no active account (no row at all,
// rather than an empty placeholder row).
function compactRowHtml(provider) {
  const active = accounts.find((a) => a.is_active && sameProvider(a, provider));
  if (!active) return null;
  const tagHtml = `<span class="compact-provider">${escapeHtml(PROVIDER_LABELS[provider] || provider)}</span>`;
  const nameHtml = `<span class="compact-name">${escapeHtml(active.name)}</span>`;
  // The full #credman-warning banner doesn't fit a compact row's height (CSS
  // hides it there — see styles.css); it's Claude-specific, so it only ever
  // surfaces on the Claude row.
  const warnHtml =
    provider === PROVIDER_CLAUDE && credmanGuardActive
      ? `<span class="compact-warn" title="Windows Credential Manager sign-in mode looks active for Claude Code — switching may not take effect.">⚠ Credential Manager</span>`
      : "";
  const u = usageCache[cacheKey(provider, active.name)];
  let bodyHtml;
  if (!u || u.state === "loading") {
    bodyHtml = `<span class="compact-empty">Loading usage…</span>`;
  } else if (u.state === "error") {
    bodyHtml = `<span class="compact-empty">Usage unavailable</span>`;
  } else if (u.state === "ratelimited") {
    bodyHtml = `<span class="compact-empty">Rate limited</span>`;
  } else {
    const meters = (u.data.buckets || []).map(compactMeterHtml).filter(Boolean).join("");
    const credits = u.data.credits_note
      ? `<span class="compact-empty">${escapeHtml(u.data.credits_note)}</span>`
      : "";
    bodyHtml = meters || credits || `<span class="compact-empty">No active limits</span>`;
  }
  return `<div class="compact-row">${tagHtml}${nameHtml}${warnHtml}${bodyHtml}</div>`;
}

function compactRows() {
  return [PROVIDER_CLAUDE, PROVIDER_CHATGPT].map(compactRowHtml).filter(Boolean);
}

// Resizes the native window to fit however many rows are currently shown —
// only while compact mode is actually on, and only when the count changed, so
// this is a cheap no-op on every ordinary usage refresh.
let lastCompactRows = null;
async function resizeCompactWindow(rows) {
  if (!compact || rows === lastCompactRows) return;
  lastCompactRows = rows;
  try {
    await invoke("set_compact", { compact: true, rows });
  } catch (e) {
    console.warn("set_compact resize failed:", e);
  }
}

function updateCompactUsage() {
  if (!compactUsageEl) return;
  const rows = compactRows();
  compactUsageEl.innerHTML = rows.length
    ? rows.join("")
    : `<div class="compact-row"><span class="compact-empty">No active account</span></div>`;
  resizeCompactWindow(Math.max(rows.length, 1));
}

function cardHtml(acc) {
  const sub = acc.subscription_type
    ? acc.subscription_type.charAt(0).toUpperCase() + acc.subscription_type.slice(1)
    : "";
  // Every backend account command is provider-scoped, and a Claude and a
  // ChatGPT account may legitimately share a name — so each button (and the
  // usage DOM id) carries the provider of the card it belongs to rather than
  // relying on the name alone.
  const provider = acc.provider || PROVIDER_CLAUDE;
  const prov = escapeAttr(provider);
  return `
    <div class="card ${acc.is_active ? "active" : ""}">
      <div class="card-head">
        <span class="name">${escapeHtml(acc.name)}</span>
        ${sub ? `<span class="badge">${sub}</span>` : ""}
        <span class="spacer"></span>
        <span class="card-actions">
          ${
            acc.is_active
              ? `<button class="switch-act active" disabled>Active</button>`
              : `<button class="switch-act" data-act="switch" data-provider="${prov}" data-name="${escapeAttr(acc.name)}">Switch</button>`
          }
          <button data-act="rename" data-provider="${prov}" data-name="${escapeAttr(acc.name)}">Rename</button>
          <button data-act="remove" data-provider="${prov}" data-name="${escapeAttr(acc.name)}">Remove</button>
        </span>
      </div>
      <div class="usage" id="usage-${prov}-${cssId(acc.name)}">${usageHtml(acc)}</div>
    </div>`;
}

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}
function escapeAttr(s) {
  return escapeHtml(s);
}
function cssId(s) {
  return String(s).replace(/[^a-zA-Z0-9_-]/g, "_");
}

function render() {
  const tabAccounts = accounts.filter((a) => sameProvider(a, currentTab));
  if (!tabAccounts.length) {
    accountsEl.innerHTML =
      currentTab === PROVIDER_CLAUDE
        ? `<div class="empty">No accounts saved yet.<br>Log into one account in Claude Code, then click “Save current account”.</div>`
        : `<div class="empty">No ChatGPT accounts saved yet.<br>Log into one account in ChatGPT / Codex, then click “Save current account”.</div>`;
  } else {
    accountsEl.innerHTML = tabAccounts.map(cardHtml).join("");
  }
  updateCompactUsage();
}

function updateUsageDom(provider, name) {
  const el = document.getElementById(`usage-${provider}-${cssId(name)}`);
  if (el) {
    const acc = accounts.find((a) => a.name === name && sameProvider(a, provider));
    if (acc) el.innerHTML = usageHtml(acc);
  }
  // The compact strip has a row for each provider's active account, so any
  // provider's active-account update can change what it shows.
  const active = accounts.find((a) => a.is_active && sameProvider(a, provider));
  if (active && active.name === name) updateCompactUsage();
}

// Local file read, no network — cheap enough to piggyback on every usage
// refresh trigger (init, interval, focus/visibility regain) so the banner
// reacts if Anthropic flips the server-side flag mid-session.
async function checkCredmanGuard() {
  try {
    credmanGuardActive = await invoke("check_credman_guard");
    credmanWarningEl.classList.toggle("hidden", !credmanGuardActive);
    updateCompactUsage(); // compact mode has no room for the full banner — see below
  } catch (e) {
    console.warn("check_credman_guard failed:", e);
  }
}

async function loadAccounts() {
  try {
    accounts = await invoke("list_accounts");
    showError("");
    // Drop cached usage (and cancel pending retries) for accounts that no
    // longer exist, so a removed account can't leave a dangling retry timer.
    Object.keys(usageCache).forEach((key) => {
      const stillExists = accounts.some((a) => cacheKey(a.provider || PROVIDER_CLAUDE, a.name) === key);
      if (!stillExists) {
        if (usageCache[key].retryTimer) clearTimeout(usageCache[key].retryTimer);
        delete usageCache[key];
      }
    });
    render();
    loadAllUsage();
    checkCredmanGuard();
  } catch (e) {
    showError(`Failed to read accounts: ${e}`);
  }
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Global serial gate for the usage API. GET /api/oauth/usage is rate-limited
// per-IP (not per-token), so ANY two calls too close together 429 — even for
// different accounts. Every real fetch (initial load, manual refresh, AND the
// 429 retry timers) is funneled through this one queue, which guarantees at
// least USAGE_MIN_GAP_MS between consecutive calls, so nothing ever fires
// back-to-back no matter how many accounts or retries are in flight.
const USAGE_MIN_GAP_MS = 2500;
let usageLastFetchAt = 0;
let usageChain = Promise.resolve();

function enqueueUsageFetch(fn) {
  const run = usageChain.then(async () => {
    const wait = USAGE_MIN_GAP_MS - (Date.now() - usageLastFetchAt);
    if (wait > 0) await sleep(wait);
    try {
      return await fn();
    } finally {
      usageLastFetchAt = Date.now();
    }
  });
  // Keep the chain alive regardless of this call's outcome (a rejection here
  // must not break spacing for the next queued fetch).
  usageChain = run.then(() => {}, () => {});
  return run;
}

// Fresh = a successful, non-stale fetch within the TTL. `fetchedAt` is the
// server-side fetch time (data.fetched_at), so stale/last-known data is never
// considered fresh and will be retried.
function usageIsFresh(provider, name) {
  const u = usageCache[cacheKey(provider, name)];
  return u && u.state === "ok" && !u.data.stale && Date.now() - u.fetchedAt < USAGE_TTL_MS;
}

// force: bypass the TTL cache (manual refresh / auto-retry after rate limit).
// Both providers go through the same get_usage command and the same serial
// queue below — Claude's /api/oauth/usage and ChatGPT's /wham/usage are
// different services, but sharing one conservative queue is simpler and safe
// for both.
async function loadUsage(provider, name, { force = false } = {}) {
  const key = cacheKey(provider, name);
  if (!force && usageIsFresh(provider, name)) return; // keep the cached bars, skip the call
  const prev = usageCache[key];
  if (prev && prev.retryTimer) clearTimeout(prev.retryTimer);

  // Keep showing existing bars while refreshing; only show "Loading…" when we
  // have nothing to show yet.
  if (!prev || prev.state !== "ok") {
    usageCache[key] = { state: "loading" };
    updateUsageDom(provider, name);
  }
  try {
    const data = await enqueueUsageFetch(() => invoke("get_usage", { provider, name }));
    // The backend serves last-known data flagged `stale` when the live fetch
    // failed (e.g. 429), so a resolved promise may still be stale.
    const fetchedAt = data.fetched_at || Date.now();
    const attempt = data.stale ? (prev && prev.retryAttempt ? prev.retryAttempt : 0) + 1 : 0;
    const sessionExpired = data.stale && /session.?expired/i.test(data.note || "");
    const entry = { state: "ok", data, fetchedAt, retryAttempt: attempt };
    if (data.stale && !sessionExpired && attempt <= RATE_LIMIT_MAX_RETRIES) {
      const delay = Math.min(30_000 * attempt, 120_000); // 30s, 60s, 90s
      entry.retryTimer = setTimeout(() => loadUsage(provider, name, { force: true }), delay);
    }
    usageCache[key] = entry;
  } catch (e) {
    // Reject only happens when the fetch failed AND there's no persisted
    // last-known usage to fall back to.
    const msg = String(e);
    if (/rate.?limit|\b429\b/i.test(msg)) {
      const attempt = (prev && prev.retryAttempt ? prev.retryAttempt : 0) + 1;
      if (attempt <= RATE_LIMIT_MAX_RETRIES) {
        const delay = Math.min(30_000 * attempt, 120_000);
        const timer = setTimeout(() => loadUsage(provider, name, { force: true }), delay);
        usageCache[key] = { state: "ratelimited", retryAttempt: attempt, retryIn: delay, retryTimer: timer };
      } else {
        usageCache[key] = { state: "ratelimited", retryAttempt: attempt, retryIn: null };
      }
    } else {
      usageCache[key] = { state: "error", message: msg };
    }
  }
  updateUsageDom(provider, name);
}

// Non-active accounts: show their last-known usage from disk with NO network
// call — so they never hit the rate-limited endpoint or show a red "retrying"
// note. No retry timer either (cancel any left over from a previous live fetch).
async function loadCachedUsage(provider, name) {
  const key = cacheKey(provider, name);
  const prev = usageCache[key];
  if (prev && prev.retryTimer) clearTimeout(prev.retryTimer);
  let data = null;
  try {
    data = await invoke("get_cached_usage", { provider, name });
  } catch (_) {
    // Ignore — just render "no usage recorded yet".
  }
  usageCache[key] = { state: "cached", data: data || null };
  updateUsageDom(provider, name);
}

// Live-refresh ONLY each provider's active account (the one actually in use).
// Non-active accounts render last-known numbers from cache — both usage
// endpoints are rate-limited, and fetching idle accounts just produced
// perpetual "rate limited, retrying…" notes on the Claude side. Each account's
// numbers refresh whenever it is the active account for its provider.
async function loadAllUsage({ force = false } = {}) {
  for (const provider of [PROVIDER_CLAUDE, PROVIDER_CHATGPT]) {
    const active = accounts.find((a) => a.is_active && sameProvider(a, provider));
    if (active) await loadUsage(provider, active.name, { force });
  }
  // Fire-and-forget: these are fast local disk reads, no network.
  for (const acc of accounts) {
    if (!acc.is_active) loadCachedUsage(acc.provider || PROVIDER_CLAUDE, acc.name);
  }
}

accountsEl.addEventListener("click", async (ev) => {
  const btn = ev.target.closest("button[data-act]");
  if (!btn) return;
  const name = btn.dataset.name;
  const act = btn.dataset.act;
  const provider = btn.dataset.provider || PROVIDER_CLAUDE;
  try {
    if (act === "switch") {
      await invoke("switch_account", { provider, name });
      showToast(`Switched to <strong>${escapeHtml(name)}</strong>.`);
    } else if (act === "remove") {
      const ok = await openConfirm({
        title: "Remove account",
        message: `Remove saved account “${name}”? This only deletes the stored copy in Switch NextUp, not your Claude account.`,
        okLabel: "Remove",
      });
      if (ok) await invoke("remove_account", { provider, name });
    } else if (act === "relogin") {
      await doLogin({ name });
    } else if (act === "rename") {
      const next = await openPrompt({ title: "Rename account", value: name, okLabel: "Rename" });
      if (!next) return;
      const trimmed = next.trim();
      if (!trimmed || trimmed === name) return;
      if (accounts.some((a) => a.name === trimmed && sameProvider(a, provider))) {
        const ok = await openConfirm({
          title: "Overwrite account",
          message: `An account named “${trimmed}” already exists. Overwrite it?`,
          okLabel: "Overwrite",
        });
        if (!ok) return;
      }
      await invoke("rename_account", { provider, old: name, new: trimmed });
    }
  } catch (e) {
    showError(String(e));
  }
});

addBtn.addEventListener("click", async () => {
  // If the currently logged-in account is (by stable identity, not tokens) one
  // we've already saved — e.g. its session expired and the user just ran
  // `claude /login` again for the same account — update that entry directly.
  // No name to type, nothing to overwrite-confirm: it's provably the same account.
  let matched = null;
  try {
    matched = await invoke("find_current_account_match", { provider: currentProvider() });
  } catch (_) {
    // Ignore — fall back to the normal name-a-new-account flow below.
  }
  if (matched) {
    try {
      await invoke("add_current_account", { provider: currentProvider(), name: matched });
      showToast(`Updated saved account <strong>${escapeHtml(matched)}</strong>.`);
    } catch (e) {
      showError(`Could not update account: ${e}`);
    }
    return;
  }

  const name = await openPrompt({
    title: "Save current account",
    placeholder: "e.g. work, personal",
    okLabel: "Save",
  });
  if (!name || !name.trim()) return;
  const trimmed = name.trim();
  if (accounts.some((a) => a.name === trimmed && sameProvider(a, currentProvider()))) {
    const ok = await openConfirm({
      title: "Overwrite account",
      message: `An account named “${trimmed}” already exists. Overwrite it with the currently logged-in account?`,
      okLabel: "Overwrite",
    });
    if (!ok) return;
  }
  try {
    await invoke("add_current_account", { provider: currentProvider(), name: trimmed });
  } catch (e) {
    showError(`Could not save current account: ${e}`);
  }
});

// In-app OAuth login ("+ Add account" and each card's "Re-login"). Both go
// through the same backend flow; `name` pins it to a known account for
// Re-login, or is omitted for "Add account", where the backend tries to match
// the logged-in identity to an existing saved account before asking us to name
// it (status "need_name").
//
// Two steps, because Anthropic refuses a loopback redirect at the moment it
// would issue the code (see login.rs): the browser lands on Anthropic's own
// callback page, which shows a code#state string the user copies back here.
let loginBusy = false;
async function doLogin({ name } = {}) {
  if (loginBusy) return;
  loginBusy = true;
  loginBtn.disabled = true;
  try {
    if (currentTab === PROVIDER_CHATGPT) {
      await doCodexLogin({ name });
      return;
    }
    await invoke("login_begin");
    const pasted = await openPrompt({
      title: name ? `Re-login ${name}` : "Finish signing in",
      message:
        "Approve the login in your browser. Anthropic then shows an authorization code — copy it and paste it here.",
      placeholder: "paste the code here",
      okLabel: "Sign in",
    });
    if (!pasted || !pasted.trim()) {
      await invoke("discard_pending_login");
      return;
    }
    showToast("Signing in…", { ms: 60_000 });
    const result = await invoke("login_submit_code", {
      name: name ?? null,
      pasted: pasted.trim(),
    });
    if (result.status === "saved") {
      showToast(`Logged in and saved <strong>${escapeHtml(result.name)}</strong>.`);
      return;
    }
    // "need_name": the logged-in identity didn't match any saved account —
    // ask for a name, holding the fresh tokens on the backend meanwhile.
    hideToast();
    const proposed = await openPrompt({
      title: "Name this account",
      value: result.suggested || "",
      placeholder: "e.g. work, personal",
      okLabel: "Save",
    });
    const trimmed = proposed ? proposed.trim() : "";
    if (!trimmed) {
      await invoke("discard_pending_login");
      return;
    }
    // Scope the clash check to this provider: the two stores are separate
    // directories and a Claude and a ChatGPT account may share a name, so an
    // unfiltered check warns about overwriting an account we would never touch.
    // `sameProvider` (not `a.provider === …`) so legacy accounts, which carry no
    // provider field and are read as Claude, still match.
    if (accounts.some((a) => sameProvider(a, PROVIDER_CLAUDE) && a.name === trimmed)) {
      const ok = await openConfirm({
        title: "Overwrite account",
        message: `A Claude account named “${trimmed}” already exists. Overwrite it with the account you just logged into?`,
        okLabel: "Overwrite",
      });
      if (!ok) {
        await invoke("discard_pending_login");
        return;
      }
    }
    await invoke("save_pending_login", { name: trimmed });
    showToast(`Logged in and saved <strong>${escapeHtml(trimmed)}</strong>.`);
  } catch (e) {
    hideToast();
    showError(`Login failed: ${e}`);
  } finally {
    loginBusy = false;
    loginBtn.disabled = false;
  }
}

// ChatGPT / Codex login. Codex registers its redirect_uri WITH the port
// (localhost:1455, fallback 1457), so the browser genuinely redirects back to
// us and the whole thing is one call — nothing for the user to copy. The one
// cost of a fixed port is that Codex's own login can be holding it; the backend
// says so in plain words when that happens.
async function doCodexLogin({ name } = {}) {
  showToast(
    name
      ? `Opening browser to re-login <strong>${escapeHtml(name)}</strong>… waiting for you to finish.`
      : "Opening browser to log into ChatGPT… waiting for you to finish.",
    { ms: 300_000 },
  );
  const result = await invoke("login_codex_account", { name: name ?? null });
  if (result.status === "saved") {
    showToast(`Logged in and saved <strong>${escapeHtml(result.name)}</strong>.`);
    return;
  }
  hideToast();
  const proposed = await openPrompt({
    title: "Name this account",
    value: result.suggested || "",
    placeholder: "e.g. work, personal",
    okLabel: "Save",
  });
  const trimmed = proposed ? proposed.trim() : "";
  if (!trimmed) {
    await invoke("discard_pending_login");
    return;
  }
  if (accounts.some((a) => sameProvider(a, PROVIDER_CHATGPT) && a.name === trimmed)) {
    const ok = await openConfirm({
      title: "Overwrite account",
      message: `A ChatGPT account named “${trimmed}” already exists. Overwrite it with the account you just logged into?`,
      okLabel: "Overwrite",
    });
    if (!ok) {
      await invoke("discard_pending_login");
      return;
    }
  }
  await invoke("save_pending_login", { name: trimmed });
  showToast(`Logged in and saved <strong>${escapeHtml(trimmed)}</strong>.`);
}

loginBtn.addEventListener("click", () => doLogin({}));

// Clear Switch NextUp's own stored data. Never touches Claude Code's credentials,
// so it does NOT log the user out of CC — only our saved copies are removed.
document.getElementById("clear-btn").addEventListener("click", async () => {
  const scope = await openChoice({
    title: "Clear Switch NextUp data",
    message:
      "Removes Switch NextUp's own saved data. This does NOT log you out of Claude Code — your currently active account stays as-is.",
    choices: [
      { label: "Clear saved accounts & usage", value: "accounts", danger: true },
      { label: "Full reset (also prefs & Start-with-Windows)", value: "all", danger: true },
    ],
  });
  if (!scope) return;
  try {
    await invoke("clear_data", { scope });
    if (scope === "all") {
      // Wipe webview-side prefs too, then reload to a clean first-run state.
      localStorage.removeItem("pinned");
      localStorage.removeItem("effect");
      location.reload();
      return;
    }
    showToast("Cleared saved accounts and usage.");
  } catch (e) {
    showError(`Could not clear data: ${e}`);
  }
});

let refreshing = false;
refreshBtn.addEventListener("click", async () => {
  if (refreshing) return; // a sweep is already running/cooling down — ignore
  refreshing = true;
  // `cooling` tints the ⟳ glyph accent for the cooldown (a static colour change,
  // no spin) and makes a hover feel inert (no lightup + not-allowed cursor).
  refreshBtn.classList.add("cooling");
  try {
    await loadAllUsage({ force: true }); // manual refresh bypasses the TTL cache
  } finally {
    // Short cooldown so rapid re-clicks can't stack sweeps and hammer the endpoint.
    setTimeout(() => {
      refreshing = false;
      refreshBtn.classList.remove("cooling");
    }, 2000);
  }
});

listen("accounts-changed", () => loadAccounts());

// ---------- auto-refresh usage (interval + on focus/visibility) ----------
// Every call funnels through loadAllUsage → loadUsage, which is TTL-gated
// (usageIsFresh) and paced by the serial queue, so none of these can hammer the
// rate-limited endpoint. While the window is hidden to the tray we skip the
// interval; showing it (focus or visibility regained) triggers an immediate,
// still-TTL-gated refresh so the numbers are current when the user looks.
setInterval(() => {
  if (document.visibilityState === "visible") {
    loadAllUsage();
    checkCredmanGuard();
  }
}, AUTO_REFRESH_MS);

document.addEventListener("visibilitychange", () => {
  if (document.visibilityState === "visible") {
    loadAllUsage();
    checkCredmanGuard();
  }
});

try {
  appWindow.onFocusChanged(({ payload: focused }) => {
    if (focused) {
      loadAllUsage();
      checkCredmanGuard();
    }
  });
} catch (e) {
  console.warn("onFocusChanged unavailable:", e);
}

loadAccounts();
