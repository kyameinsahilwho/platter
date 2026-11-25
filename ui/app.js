// Platter — UI. Vanilla ES module; talks to the Rust backend through window.__TAURI__.
import { ICONS } from "./icons.js";

if (!window.__TAURI__) {
  // Plain browser (design preview): use the fake backend.
  const m = await import("./mock.js");
  m.installMock();
}
const T = window.__TAURI__;
const invoke = (cmd, args) => T.core.invoke(cmd, args);
const listen = (name, cb) => T.event.listen(name, (e) => cb(e.payload));
// Performance timeline (only records when PLATTER_PERF is set; see src/perf.rs).
const perf = (name) => { try { T.core.invoke("perf_mark", { name }).catch(() => {}); } catch {} };
perf("js:script-start");
document.fonts?.ready.then(() => perf("js:fonts-ready"));

/* ------------------------------------------------------------------ helpers */

const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];
const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
const icon = (name, cls = "") => `<svg class="i ${cls}" viewBox="0 0 24 24" aria-hidden="true">${ICONS[name] || ""}</svg>`;
const errText = (e) => (typeof e === "string" ? e : e?.message || "Something went wrong.");

// Replace an element's markup only when it actually changed, so periodic refreshes (volume polling,
// clean-up progress, background data) don't rebuild DOM, restyle or lose hover for nothing.
function patch(el, html) {
  if (!el || el._h === html) return false;
  el.innerHTML = html;
  el._h = html;
  return true;
}
function fromHTML(html) {
  const t = document.createElement("template");
  t.innerHTML = html.trim();
  return t.content.firstElementChild;
}
// Keyed list update: entries are [key, html]. Unchanged children are kept as they are (same nodes,
// same focus, same hover); changed ones are swapped; order follows `entries`.
function patchKeyed(parent, entries) {
  const old = new Map();
  for (const el of parent.children) if (el.dataset.key != null) old.set(el.dataset.key, el);
  let prev = null;
  for (const [key, html] of entries) {
    let el = old.get(key);
    old.delete(key);
    if (!el || el._h !== html) {
      const n = fromHTML(html);
      n.dataset.key = key; n._h = html;
      if (el) el.replaceWith(n);
      el = n;
    }
    const want = prev ? prev.nextElementSibling : parent.firstElementChild;
    if (want !== el) parent.insertBefore(el, want);
    prev = el;
  }
  for (const el of old.values()) el.remove();
  for (const el of [...parent.children]) if (el.dataset.key == null) el.remove();
}
const idle = (fn, timeout = 2000) => (window.requestIdleCallback ? requestIdleCallback(fn, { timeout }) : setTimeout(fn, 1));

function fmtBytes(n, { exact = false } = {}) {
  n = Number(n) || 0;
  if (exact) return `${n.toLocaleString("en-US")} bytes`;
  const units = ["bytes", "KB", "MB", "GB", "TB"];
  let i = 0;
  while (n >= 1024 && i < units.length - 1) { n /= 1024; i++; }
  if (i === 0) return `${n} ${n === 1 ? "byte" : "bytes"}`;
  const digits = n >= 100 ? 0 : 1;
  return `${n.toFixed(digits)} ${units[i]}`;
}
const fmtInt = (n) => Number(n || 0).toLocaleString("en-US");
const pct = (a, b) => (b > 0 ? Math.round((a / b) * 100) : 0);

function relTime(iso) {
  const t = new Date(iso).getTime();
  if (!t) return "";
  const d = (Date.now() - t) / 1000;
  const fut = d < 0;
  const a = Math.abs(d);
  const out = (v, u) => (fut ? `in ${v} ${u}${v === 1 ? "" : "s"}` : `${v} ${u}${v === 1 ? "" : "s"} ago`);
  if (a < 90) return fut ? "in a moment" : "just now";
  if (a < 3600) return out(Math.round(a / 60), "minute");
  if (a < 86400) return out(Math.round(a / 3600), "hour");
  if (a < 86400 * 14) return out(Math.round(a / 86400), "day");
  if (a < 86400 * 70) return out(Math.round(a / 604800), "week");
  return out(Math.round(a / 2629800), "month");
}
// A date-only "YYYY-MM-DD" (an app's install date) is a calendar day: read it as local midnight,
// not UTC midnight, or it shows a day early west of UTC.
const toDate = (iso) => { const m = /^(\d{4})-(\d{2})-(\d{2})$/.exec(iso); return m ? new Date(+m[1], +m[2] - 1, +m[3]) : new Date(iso); };
const shortDate = (iso) => { const d = toDate(iso); return d.toLocaleDateString("en-US", { month: "short", day: "numeric", year: d.getFullYear() === new Date().getFullYear() ? undefined : "numeric" }); };
const daysSince = (iso) => (Date.now() - new Date(iso).getTime()) / 86400000;

const driveName = (v) => `${v.label || (v.kind === "removable" ? "USB drive" : v.kind === "network" ? "Network drive" : "Local Disk")} (${v.letter}:)`;
function mediaLabel(v) {
  if (v.media === "ssd") return v.bus && v.bus !== "SATA" ? `${v.bus} SSD` : "SSD";
  if (v.media === "hdd") return "Hard disk";
  if (v.kind === "removable") return v.bus === "USB" ? "USB flash drive" : "Removable drive";
  return v.bus || "Drive";
}
const driveIcon = (v) => (v.kind === "removable" ? "usb" : v.kind === "optical" ? "disc-3" : "hard-drive");
const fullness = (v) => (v.total ? (v.total - v.free) / v.total : 0);
const fullTone = (v) => (fullness(v) >= 0.9 ? "is-danger" : fullness(v) >= 0.8 ? "is-warn" : "");

const LABEL_BAD_RE = /[*?/\\|,;:+=<>[\]"]/;
const LABEL_BAD = "Drive names can't contain * ? / \\ | , ; : + = < > [ ] or quotes.";

const CATS = ["Video", "Images", "Audio", "Documents", "Archives & installers", "Apps & system", "Developer", "Other"];
const catVar = (c) => `var(--cat-${c ?? 7})`;

/* ------------------------------------------------------------------ state */

const S = {
  sys: { elevated: false, systemDrive: "C" },
  volumes: [],
  disks: null, disksError: null, disksLoading: false,
  history: null, schedule: null,
  route: { view: "drive", letter: "C", tab: "overview" },
  maps: {},        // letter -> { data, error, loading, drawn }
  scan: { status: "idle" },
  // measuredAt: ms epoch of the result shown (from the cache at launch, or the last measure).
  cleanup: { status: "idle", items: [], selected: new Set(), states: {}, touched: false, measuredAt: 0, stale: false, background: false },
  opt: { running: null, results: {} },
  checks: {},      // letter -> { status: "running" } | result | { error }
  drafts: { label: {}, letter: {} }, // letter -> what the user typed/picked on Manage, kept across re-renders
  cleanSub: "caches", // last Clean up section shown (caches | apps | dupes)
  // Uninstall apps. left = the leftovers screen (one app's, or orphans) when it's open.
  apps: { status: "idle", list: [], error: null, q: "", sort: "name", busy: null, notice: null, left: null },
  // Duplicates. status: setup | running | done | error.
  dupes: { status: "setup", roots: null, picked: null, custom: [], opts: null, scanId: 0, progress: null, result: null,
    index: new Map(), sel: new Set(), section: "files", shown: 120, error: null, finishedAt: 0, deleting: false, lastDelete: null, early: null },
};

function vol(letter) { return S.volumes.find((v) => v.letter === letter); }
function diskFor(letter) { return S.disks?.find((d) => d.partitions?.some((p) => p.letter === letter)); }

/* ------------------------------------------------------------------ theme */

const THEME_KEY = "platter.theme";
function getTheme() { try { return localStorage.getItem(THEME_KEY) || "system"; } catch { return "system"; } }
function setTheme(t) {
  try { localStorage.setItem(THEME_KEY, t); } catch {}
  syncThemePref(t);
  if (t === "system") delete document.documentElement.dataset.theme; else document.documentElement.dataset.theme = t;
  paintThemeSwitch();
  redrawMap();
}
// The backend remembers the theme too, so the native window background matches before the first paint.
// Older backends don't have the command: ignore that.
const THEME_SYNC_KEY = "platter.themeSynced";
function syncThemePref(t = getTheme()) {
  try { invoke("set_theme_pref", { theme: t }).then(() => { try { localStorage.setItem(THEME_SYNC_KEY, t); } catch {} }, () => {}); } catch {}
}
function paintThemeSwitch() {
  const cur = getTheme();
  const ic = { light: "sun", system: "laptop", dark: "moon" };
  $$("[data-theme-choice]").forEach((b) => {
    b.innerHTML = icon(ic[b.dataset.themeChoice]);
    b.setAttribute("aria-checked", String(b.dataset.themeChoice === cur));
  });
}
matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => redrawMap());

/* ------------------------------------------------------------------ toasts & dialogs */

function toast(msg, { tone = "ok", action, onAction, ms = 5200 } = {}) {
  const el = document.createElement("div");
  el.className = `toast ${tone === "bad" ? "is-bad" : ""}`;
  el.innerHTML = `${icon(tone === "bad" ? "circle-alert" : "circle-check")}<span class="msg">${esc(msg)}</span>${action ? `<button class="link-btn" type="button">${esc(action)}</button>` : ""}`;
  if (action) el.querySelector("button").onclick = () => { onAction?.(); el.remove(); };
  $("#toasts").append(el);
  setTimeout(() => el.remove(), ms);
}

function dialog({ title, body, confirm = "Continue", tone = "primary", cancel = "Cancel" }) {
  const d = $("#dialog");
  d.innerHTML = `<form method="dialog">
      <div class="dialog-body"><h2>${esc(title)}</h2>${body}</div>
      <div class="dialog-actions">
        <button class="btn ghost" value="cancel">${esc(cancel)}</button>
        <button class="btn ${tone}" value="ok" autofocus>${esc(confirm)}</button>
      </div></form>`;
  // Esc closes with no result and keeps the old returnValue, so clear it or a past "ok" would count.
  d.returnValue = "";
  d.showModal();
  return new Promise((res) => d.addEventListener("close", () => res(d.returnValue === "ok"), { once: true }));
}

async function askElevate(reason) {
  const ok = await dialog({
    title: "Restart as administrator?",
    body: `<p>${esc(reason)}</p><p>Platter will close and reopen with administrator rights. Windows will ask you to confirm.</p>`,
    confirm: "Restart as administrator",
  });
  if (!ok) return;
  try {
    const accepted = await invoke("relaunch_elevated");
    if (!accepted) toast("Windows didn't grant administrator rights, so Platter is still in standard mode.", { tone: "bad" });
  } catch (e) { toast(errText(e), { tone: "bad" }); }
}

/* ------------------------------------------------------------------ data loading */

async function loadVolumes() {
  try {
    S.volumes = await invoke("list_volumes");
  } catch (e) { toast(errText(e), { tone: "bad" }); }
}
// Each loader resolves to true when what's on screen should change (new data, or an error to show).
// When a refresh fails but cached data is showing, the cached data stays.
const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);
async function loadDisks() {
  const before = [S.disks, S.disksError];
  S.disksLoading = true; S.disksError = null;
  try { const d = await invoke("list_disks"); if (!same(d, S.disks)) S.disks = d; }
  catch (e) { S.disksError = errText(e); }
  S.disksLoading = false;
  return before[0] !== S.disks || before[1] !== S.disksError;
}
async function loadHistory() {
  const [h0, s0] = [S.history, S.schedule];
  S.historyLoading = true;
  const [h, s] = await Promise.allSettled([invoke("optimize_history"), invoke("optimize_schedule")]);
  if (h.status === "fulfilled") { if (!same(h.value, S.history)) S.history = h.value; } else if (S.history == null) S.history = [];
  if (s.status === "fulfilled" && !same(s.value, S.schedule)) S.schedule = s.value;
  S.historyLoading = false;
  return h0 !== S.history || s0 !== S.schedule || !S.schedule; // no schedule: swap "Reading…" for the error
}
async function loadMap(letter, force = false) {
  const v = vol(letter);
  if (!v || !v.ready) return;
  const m = S.maps[letter];
  if (m && (m.loading || (m.data && !force))) return;
  S.maps[letter] = { ...(m || {}), loading: true, error: null };
  try {
    const data = await invoke("allocation_map", { letter, cells: MAP_COLS * MAP_ROWS });
    S.maps[letter] = { data, loading: false, drawn: m?.drawn || false };
  } catch (e) {
    S.maps[letter] = { error: errText(e), loading: false };
  }
  if (S.route.view === "drive" && S.route.letter === letter) renderMapSection();
}

/* ------------------------------------------------------------------ rail */

function renderRail() {
  const r = S.route;
  const fixed = S.volumes.filter((v) => v.kind !== "network" && v.kind !== "optical");
  const other = S.volumes.filter((v) => v.kind === "network" || v.kind === "optical");
  const cleanTotal = selectedTotal();
  const driveBtn = (v) => {
    const cur = r.view === "drive" && r.letter === v.letter;
    return `<button class="nav-item drive-item" data-action="go-drive" data-letter="${v.letter}" ${cur ? 'aria-current="page"' : ""}>
      ${icon(driveIcon(v))}
      <span class="nav-label">${esc(driveName(v))}</span>
      <span></span>
      ${v.ready && v.total ? `<div class="meter ${fullTone(v)}" aria-hidden="true"><span style="width:${(fullness(v) * 100).toFixed(1)}%"></span></div>
      <span class="drive-free num">${fmtBytes(v.free)} free of ${fmtBytes(v.total)}</span>` : `<span class="drive-free">${notReadyShort(v)}</span>`}
    </button>`;
  };
  const measured = S.cleanup.measuredAt ? ` title="Measured ${esc(relTime(S.cleanup.measuredAt))}"` : "";
  restoringFocus(() => patch($("#rail-nav"), `
    <button class="nav-item" data-action="go" data-view="pc" ${r.view === "pc" ? 'aria-current="page"' : ""}>${icon("monitor")}<span class="nav-label">This PC</span></button>
    <button class="nav-item" data-action="go" data-view="cleanup" ${r.view === "cleanup" ? 'aria-current="page"' : ""}>${icon("brush-cleaning")}<span class="nav-label">Clean up</span>
      ${S.cleanup.status === "analyzing" && !S.cleanup.items.length ? `<span class="nav-badge">Measuring…</span>` : cleanTotal > 0 ? `<span class="nav-badge is-good num"${measured}>${fmtBytes(cleanTotal)}</span>` : ""}</button>
    <div class="rail-group">Drives</div>
    ${fixed.map(driveBtn).join("")}
    ${other.length ? `<div class="rail-group">Network and optical</div>${other.map(driveBtn).join("")}` : ""}
  `));
  // null: elevated as another Windows account, where the setting can't be reached (change it in standard mode).
  const askAtStart = S.sys.askAdminAtStart === null ? "" : `<label class="elev-auto" title="Windows asks for administrator rights each time Platter opens">
        <input type="checkbox" id="ask-admin" ${S.sys.askAdminAtStart !== false ? "checked" : ""} /><span>Ask at startup</span></label>`;
  // Also called outside render() (polling ticks): keep focus on the checkbox if this redraws it.
  restoringFocus(() => patch($("#elevation"), S.sys.elevated
    ? `<div class="elev is-admin">${icon("shield-check")}<span class="elev-title">Administrator</span><span class="elev-text">Every tool is available.</span>${askAtStart}</div>`
    : `<div class="elev">${icon("shield")}<span class="elev-title">Standard mode</span><span class="elev-text">Trim, defragment and some clean-up need administrator rights.</span>
        <button class="link-btn" data-action="elevate">Restart as administrator</button>${askAtStart}</div>`));
}

async function setAskAdmin(enabled) {
  S.sys.askAdminAtStart = enabled;
  try {
    await invoke("set_ask_admin", { enabled });
    toast(enabled ? "Platter will ask for administrator rights each time it opens." : "Platter will open in standard mode.");
  } catch (e) {
    // Put the box back as it was (the rail's HTML may be unchanged, so patch() wouldn't redraw it).
    S.sys.askAdminAtStart = !enabled;
    const cb = $("#ask-admin");
    if (cb) cb.checked = !enabled;
    toast(errText(e), { tone: "bad" });
  }
}

/* ------------------------------------------------------------------ routing & render */

function go(route) {
  const prev = S.route;
  S.route = { ...route };
  if (route.view === "drive" && !route.tab) S.route.tab = prev.view === "drive" ? prev.tab : "overview";
  if (route.view === "cleanup") S.route.sub = CLEAN_SUBS.some(([k]) => k === route.sub) ? route.sub : S.cleanSub;
  render({ resetScroll: true });
  saveRoute();
  if (S.route.view === "cleanup") enterCleanSub();
}

// The last place the user was (view, drive, tab) is restored at the next launch.
const ROUTE_KEY = "platter.route";
const TABS = ["overview", "space", "optimize", "manage"];
function saveRoute() {
  const r = S.route;
  if (r.view === "cleanup") S.cleanSub = cleanSub();
  try { localStorage.setItem(ROUTE_KEY, JSON.stringify(r.view === "drive" ? { view: "drive", letter: r.letter, tab: r.tab } : r.view === "cleanup" ? { view: "cleanup", sub: cleanSub() } : { view: r.view })); } catch {}
}
function savedRoute() {
  try {
    const r = JSON.parse(localStorage.getItem(ROUTE_KEY) || "null");
    if (r?.view === "pc") return { view: r.view };
    if (r?.view === "cleanup") return { view: "cleanup", sub: CLEAN_SUBS.some(([k]) => k === r.sub) ? r.sub : "caches" };
    if (r?.view === "drive" && vol(r.letter)) return { view: "drive", letter: r.letter, tab: TABS.includes(r.tab) ? r.tab : "overview" };
  } catch {}
  return null;
}
function setTab(tab) {
  S.route.tab = tab;
  if (tab === "manage") ensureFreeLetters();
  renderPanel();
  saveRoute();
}

// Re-rendering replaces elements wholesale. Remember which control had focus (and the caret in a
// text field) and put focus back on its replacement, so keyboard users and typists don't lose their place.
function focusKey(el) {
  if (!el || el === document.body || !el.isConnected) return null;
  if (el.id) return `#${CSS.escape(el.id)}`;
  const a = el.dataset?.action;
  if (!a) return null;
  for (const k of ["id", "tab", "sub", "letter", "view", "path", "mode", "v", "i", "gid"]) {
    const val = el.dataset[k];
    if (val != null) return `[data-action="${CSS.escape(a)}"][data-${k}="${CSS.escape(val)}"]`;
  }
  return `[data-action="${CSS.escape(a)}"]`;
}
function restoringFocus(fn) {
  const active = document.activeElement;
  const key = focusKey(active);
  const caret = key && active.matches?.("input.input") ? [active.selectionStart, active.selectionEnd] : null;
  fn();
  if (!key || active.isConnected) return;
  const next = $(key);
  if (!next || next.disabled) return;
  next.focus({ preventScroll: true });
  if (caret) try { next.setSelectionRange(caret[0], caret[1]); } catch {}
}

function render({ resetScroll = false } = {}) {
  if (S.route.view !== "pc" && S.route.view !== "cleanup" && !vol(S.route.letter)) S.route = { view: "pc" };
  restoringFocus(() => {
    renderRail();
    const main = $("#main");
    const top = main.scrollTop;
    const r = S.route;
    if (r.view === "pc") main.innerHTML = viewPC();
    else if (r.view === "cleanup") {
      // Already showing Clean up: update only the parts that changed (progress, sizes, one checkbox).
      if (!resetScroll && $("#clean-page")?.dataset.sub === cleanSub()) updateCleanup();
      else { main.innerHTML = viewCleanup(); updateCleanup(); }
    }
    else { main.innerHTML = viewDrive(vol(r.letter)); afterPanel(); }
    if (resetScroll) main.scrollTop = 0; else if (main.scrollTop !== top) main.scrollTop = top;
  });
}

function renderPanel() {
  const p = $("#panel");
  const v = vol(S.route.letter);
  if (!p || !v || S.route.view !== "drive") return render();
  restoringFocus(() => {
    const main = $("#main"); const top = main.scrollTop;
    p.innerHTML = panelFor(v);
    $$(".tab").forEach((t) => t.setAttribute("aria-selected", String(t.dataset.tab === S.route.tab)));
    afterPanel();
    main.scrollTop = top;
  });
}

function afterPanel() {
  if (S.route.view === "drive" && S.route.tab === "manage" && !S.freeLetters) ensureFreeLetters();
  if (S.route.view === "drive" && S.route.tab === "overview") { renderMapSection(); loadMap(S.route.letter); }
  if (S.route.view === "drive" && S.route.tab === "space" && S.scan.status === "done" && S.scan.letter === S.route.letter) drawTreemap();
}

/* ------------------------------------------------------------------ drive view */

function viewDrive(v) {
  const t = S.route.tab;
  const facts = [
    `<span class="chip">${icon(driveIcon(v))}${esc(mediaLabel(v))}</span>`,
    v.fileSystem ? `<span class="chip">${esc(v.fileSystem)}</span>` : "",
    v.isSystem ? `<span class="chip is-pine">Windows is installed here</span>` : "",
    v.readOnly ? `<span class="chip is-warn">Read-only</span>` : "",
  ].join("");
  const tab = (id, label) => `<button class="tab" role="tab" data-action="tab" data-tab="${id}" aria-selected="${t === id}" title="${label} (${TABS.indexOf(id) + 1})" aria-keyshortcuts="${TABS.indexOf(id) + 1}">${label}</button>`;
  return `<div class="page">
    <header class="page-head">
      <div>
        <h1 class="page-title">${esc(driveName(v))}</h1>
        <div class="chips">${facts}</div>
      </div>
      <div class="head-actions">
        <button class="btn icon" data-action="refresh" aria-label="Refresh" title="Refresh drive info (F5)" aria-keyshortcuts="F5 Control+R">${icon("refresh-cw")}</button>
        <button class="btn" data-action="open-path" data-path="${esc(v.letter)}:\\">${icon("folder-open")}Open in Explorer</button>
      </div>
    </header>
    <section class="capacity" id="capacity">${capacityHTML(v)}</section>
    <nav class="tabs" role="tablist" aria-label="Drive sections">
      ${tab("overview", "Overview")}${tab("space", "What's using space")}${tab("optimize", v.media === "ssd" ? "Trim" : "Optimize")}${tab("manage", "Manage")}
    </nav>
    <div id="panel" role="tabpanel">${panelFor(v)}</div>
  </div>`;
}

function reclaimFor(v) {
  if (!v.isSystem || !S.cleanup.items.length) return 0;
  return Math.min(selectedTotal(), v.total - v.free);
}

// Why a drive can't be read. Volume.status is optional (older backends only send `ready`).
const NOT_READY_TEXT = {
  locked: ["lock", "This drive is locked by BitLocker. Unlock it in Explorer, then refresh."],
  raw: ["info", "This drive isn't formatted, so Windows can't store files on it yet."],
  unavailable: ["info", "Windows can't reach this drive right now. Reconnect it, then refresh."],
  "not-ready": ["info", "This drive isn't ready. Insert media or reconnect it, then refresh."],
};
const notReadyShort = (v) => ({ locked: "Locked", raw: "Not formatted", unavailable: "Unavailable" })[v.status] || "Not ready";

function capacityHTML(v) {
  if (!v.ready || (v.status && v.status !== "ok")) {
    const [ic, text] = NOT_READY_TEXT[v.status] || NOT_READY_TEXT["not-ready"];
    return `<div class="map-empty">${icon(ic)}${esc(text)}</div>`;
  }
  const used = v.total - v.free;
  const rec = reclaimFor(v);
  const tone = fullTone(v);
  return `
    <div class="readout"><span class="big">${fmtBytes(v.free)}</span><span class="of">free of ${fmtBytes(v.total)}</span></div>
    <div class="capbar-wrap">
      <div class="capbar" role="img" aria-label="${pct(used, v.total)}% used">
        <span class="used ${tone}" style="width:${((used - rec) / v.total) * 100}%"></span>
        ${rec ? `<span class="reclaim" style="width:${(rec / v.total) * 100}%"></span>` : ""}
      </div>
      <div class="cap-legend">
        <span><i class="swatch used"></i>Used <b>${fmtBytes(used)}</b> (${pct(used, v.total)}%)</span>
        ${rec ? `<span><i class="swatch reclaim"></i>Can be cleaned <b>${fmtBytes(rec)}</b></span>` : ""}
        <span><i class="swatch free"></i>Free <b>${fmtBytes(v.free)}</b></span>
      </div>
    </div>`;
}

/* ---------- allocation map ---------- */

const MAP_COLS = 128, MAP_ROWS = 16;
let mapHover = -1;

function renderMapSection() {
  const host = $("#map");
  const v = vol(S.route.letter);
  if (!host || !v) return;
  const m = S.maps[v.letter];
  if (!v.ready) { host.innerHTML = ""; return; }
  if (!m || m.loading) {
    host.innerHTML = `<div class="map-canvas-wrap"><canvas id="map-canvas" height="0"></canvas><div class="map-empty">${icon("loader-circle", "spin")}Reading the allocation map…</div></div>`;
    return;
  }
  if (m.error) {
    const needsAdmin = /administrator/.test(m.error) && !S.sys.elevated;
    host.innerHTML = `<div class="map-canvas-wrap is-ghost"><canvas id="map-canvas" aria-hidden="true"></canvas>
      <div class="map-overlay">${icon(needsAdmin ? "lock" : "info")}<span>${needsAdmin ? "The allocation map shows where data physically sits on the drive. Windows only lets administrators read it." : esc(m.error)}</span>
      ${needsAdmin ? `<button class="btn sm" data-action="elevate">Restart as administrator</button>` : ""}</div></div>
      <div class="map-foot"><span>${needsAdmin ? "Everything else on this page works in standard mode." : ""}</span></div>`;
    drawGhostMap();
    return;
  }
  const d = m.data;
  host.innerHTML = `
    <div class="map-canvas-wrap"><canvas id="map-canvas" aria-label="Each block is about ${fmtBytes(d.bytesPerCell)} of the drive, in physical order from start to end"></canvas></div>
    <div class="map-foot">
      <span id="map-caption">${mapCaption(d)}</span>
      <span class="map-legend"><span><i class="swatch free"></i>Free</span><span><i class="swatch low"></i>Partly used</span><span><i class="swatch mid"></i>Mostly used</span><span><i class="swatch used"></i>Full</span></span>
    </div>`;
  const c = $("#map-canvas");
  mapHover = mapHoverWant = -1; // a new canvas starts without hover
  c.addEventListener("mousemove", onMapHover);
  c.addEventListener("mouseleave", () => setMapHover(-1));
  drawMap(!m.drawn && !document.hidden);
  m.drawn = true;
}

function mapCaption(d) {
  return `Each block is about <b class="num">${fmtBytes(d.bytesPerCell)}</b> of the drive, from its start (top left) to its end. Free space is split into <b class="num">${fmtInt(d.freeRuns)}</b> pieces; the largest is <b class="num">${fmtBytes(d.largestFreeRun)}</b>.`;
}

// Hover is applied at most once per frame, and only repaints the cell that stopped being hovered and
// the one that started (see paintMapHover); the other 2047 cells are never repainted on mousemove.
let mapHoverWant = -1, mapHoverRaf = 0;
function onMapHover(e) {
  const m = S.maps[S.route.letter]; if (!m?.data) return;
  const c = e.currentTarget; const r = c.getBoundingClientRect();
  const col = Math.floor(((e.clientX - r.left) / r.width) * MAP_COLS);
  const row = Math.floor(((e.clientY - r.top) / r.height) * MAP_ROWS);
  const i = row * MAP_COLS + col;
  if (col < 0 || col >= MAP_COLS || i < 0 || i >= m.data.cells.length) return;
  setMapHover(i);
}
function setMapHover(i) {
  mapHoverWant = i;
  if (!mapHoverRaf) mapHoverRaf = requestAnimationFrame(applyMapHover);
}
function applyMapHover() {
  mapHoverRaf = 0;
  const i = mapHoverWant, prev = mapHover;
  if (i === prev) return;
  mapHover = i;
  const m = S.maps[S.route.letter]; const cap = $("#map-caption");
  if (!m?.data || !cap) return;
  if (i < 0) cap.innerHTML = mapCaption(m.data);
  else {
    const fill = Math.round((m.data.cells[i] / 255) * 100);
    cap.innerHTML = `Block ${fmtInt(i + 1)} of ${fmtInt(m.data.cells.length)}: <b class="num">${fill}% used</b>, starting <b class="num">${fmtBytes(i * m.data.bytesPerCell)}</b> into the drive.`;
  }
  paintMapHover(prev, i);
}

// Geometry, colours and state of the map as last drawn (used by hover and resize).
let mapView = null;
function mapGeometry(c) {
  const dpr = window.devicePixelRatio || 1, w = c.clientWidth, pitch = w / MAP_COLS;
  return { dpr, w, pitch, gap: Math.max(1.5, pitch * 0.22), h: Math.round(pitch * MAP_ROWS) };
}
function sizeCanvas(c, geo) {
  const h = `${geo.h}px`;
  if (c.style.height !== h) c.style.height = h;
  const W = Math.round(geo.w * geo.dpr), H = Math.round(geo.h * geo.dpr);
  if (c.width !== W || c.height !== H) { c.width = W; c.height = H; }
  const g = c.getContext("2d");
  g.setTransform(geo.dpr, 0, 0, geo.dpr, 0, 0);
  return g;
}

function redrawMap() {
  if (!$("#map-canvas")) return;
  if (S.maps[S.route.letter]?.data) drawMap(false); else if (S.maps[S.route.letter]?.error) drawGhostMap();
}

function drawGhostMap() {
  const c = $("#map-canvas"); if (!c) return;
  const free = getComputedStyle(document.documentElement).getPropertyValue("--cell-free").trim();
  const geo = mapGeometry(c); const g = sizeCanvas(c, geo);
  const { pitch, gap } = geo;
  const path = new Path2D();
  for (let i = 0; i < MAP_COLS * MAP_ROWS; i++) path.roundRect((i % MAP_COLS) * pitch, Math.floor(i / MAP_COLS) * pitch, pitch - gap, pitch - gap, 1.5);
  g.clearRect(0, 0, geo.w, geo.h);
  g.fillStyle = free; g.fill(path);
  mapView = { canvas: c, ghost: true, w: geo.w, dpr: geo.dpr };
}

// Resizes come in bursts while the window is dragged: redraw once per frame, and only what changed size.
let resizeRaf = 0, treemapResizeT = 0, lastTreemapSize = "";
window.addEventListener("resize", () => {
  if (!resizeRaf) resizeRaf = requestAnimationFrame(() => {
    resizeRaf = 0;
    const c = $("#map-canvas");
    if (c && (!mapView || mapView.canvas !== c || c.clientWidth !== mapView.w || (window.devicePixelRatio || 1) !== mapView.dpr)) redrawMap();
  });
  // The treemap is DOM: rebuild it once the size settles.
  clearTimeout(treemapResizeT);
  treemapResizeT = setTimeout(() => {
    const host = $("#treemap");
    if (host && `${host.clientWidth}x${host.clientHeight}` !== lastTreemapSize) drawTreemap();
  }, 90);
});

function drawMap(animate) {
  const c = $("#map-canvas"); const m = S.maps[S.route.letter];
  if (!c || !m?.data) return;
  const cs = getComputedStyle(document.documentElement);
  const col = (n) => cs.getPropertyValue(n).trim();
  const colors = [col("--cell-free"), col("--cell-low"), col("--cell-mid"), col("--cell-full")];
  const geo = mapGeometry(c); const g = sizeCanvas(c, geo);
  const { w, h, pitch, gap } = geo;
  const rad = Math.min(1.5, pitch * 0.15);
  const cells = m.data.cells;
  const bucket = (b) => (b === 0 ? 0 : b < 90 ? 1 : b < 230 ? 2 : 3);
  const view = { canvas: c, g, ready: false, ...geo, rad, hoverCol: col("--warn"), animating: false, colorOf: (i) => colors[bucket(cells[i])] };
  mapView = view;
  // One path per colour: four fills instead of 2048.
  const paint = (upto) => {
    const paths = [new Path2D(), new Path2D(), new Path2D(), new Path2D()];
    for (let i = 0; i < cells.length; i++) {
      paths[i < upto ? bucket(cells[i]) : 0].roundRect((i % MAP_COLS) * pitch, Math.floor(i / MAP_COLS) * pitch, pitch - gap, pitch - gap, rad);
    }
    g.clearRect(0, 0, w, h);
    for (let k = 0; k < 4; k++) { g.fillStyle = colors[k]; g.fill(paths[k]); }
  };
  const finish = () => {
    view.animating = false;
    paint(cells.length);
    view.ready = true;
    if (mapHover >= 0) paintMapHover(-1, mapHover);
  };
  const reduce = matchMedia("(prefers-reduced-motion: reduce)").matches;
  if (!animate || reduce) return finish();
  // The one orchestrated moment: blocks settle in physical order, like a drive being read.
  view.animating = true;
  const t0 = performance.now(), dur = 900;
  const step = (t) => {
    if (mapView !== view || $("#map-canvas") !== c) return;
    const p = Math.min(1, (t - t0) / dur);
    if (p >= 1) return finish();
    paint(Math.floor((1 - Math.pow(1 - p, 3)) * cells.length));
    requestAnimationFrame(step);
  };
  requestAnimationFrame(step);
}

// Repaint just two cells: the one that stopped being hovered gets its own colour back, the new one
// is highlighted. Cells are at least 1.5px apart, so clearing half a pixel around one never touches
// its neighbours.
function paintMapHover(prev, next) {
  const v = mapView;
  if (!v || !v.ready || v.canvas !== $("#map-canvas")) return;
  const { g, pitch, gap, rad } = v;
  const cell = (i, color) => {
    const x = (i % MAP_COLS) * pitch, y = Math.floor(i / MAP_COLS) * pitch, s = pitch - gap;
    g.clearRect(x - 0.5, y - 0.5, s + 1, s + 1);
    g.fillStyle = color;
    g.beginPath();
    g.roundRect(x, y, s, s, rad);
    g.fill();
  };
  if (prev >= 0) cell(prev, v.colorOf(prev));
  if (next >= 0) cell(next, v.hoverCol);
}

/* ---------- panels ---------- */

function panelFor(v) {
  switch (S.route.tab) {
    case "space": return tabSpace(v);
    case "optimize": return tabOptimize(v);
    case "manage": return tabManage(v);
    default: return tabOverview(v);
  }
}

function lastOptimize(letter, actions) {
  return (S.history || []).find((h) => h.letter === letter && h.ok && actions.includes(h.action));
}

function suggestions(v) {
  const out = [];
  const freePct = v.total ? (v.free / v.total) * 100 : 100;
  const rec = v.isSystem ? selectedTotal() : 0;
  if (v.ready && freePct < 10) {
    out.push({ tone: "danger", icon: "triangle-alert", title: `Only ${Math.round(freePct)}% free`,
      text: `Windows needs room for updates, virtual memory and temporary files. Aim to free at least ${fmtBytes(v.total * 0.15 - v.free)}.`,
      action: v.isSystem ? ["Clean up", "go-cleanup"] : ["See what's using space", "tab-space"] });
  } else if (v.ready && freePct < 20) {
    out.push({ tone: "warn", icon: "triangle-alert", title: `Getting full: ${Math.round(freePct)}% free`,
      text: "Drives slow down and updates can fail when space runs low.",
      action: ["See what's using space", "tab-space"] });
  }
  const disk = diskFor(v.letter);
  if (disk && disk.health && disk.health !== "Healthy" && disk.health !== "Unknown") {
    out.push({ tone: "danger", icon: "circle-alert", title: `Windows reports this disk as ${disk.health.toLowerCase()}`,
      text: "Back up anything important on it now, then check the disk's details on This PC.", action: ["View disk", "go-pc"] });
  }
  if (rec >= 512 * 1024 * 1024) {
    out.push({ tone: "info", icon: "brush-cleaning", title: `${fmtBytes(rec)} of temporary files can go`,
      text: "Caches, leftovers and update downloads that Windows and your apps recreate when needed.", action: ["Review clean-up", "go-cleanup"] });
  }
  if (["NTFS", "ReFS"].includes(v.fileSystem) && S.history) {
    const isSsd = v.media === "ssd";
    const last = lastOptimize(v.letter, isSsd ? ["Trim"] : ["Defragment"]);
    const label = isSsd ? "trimmed" : "defragmented";
    if (!last || daysSince(last.time) > 31) {
      out.push({ tone: "warn", icon: "clock", title: last ? `Not ${label} in ${relTime(last.time).replace(" ago", "")}` : `No record of this drive being ${label}`,
        text: isSsd ? "A trim helps the SSD keep its write speed up." : "Defragmenting brings scattered files back together.",
        action: [isSsd ? "Go to Trim" : "Go to Optimize", "tab-optimize"] });
    } else {
      out.push({ tone: "ok", icon: "circle-check", title: `${isSsd ? "Trimmed" : "Defragmented"} ${relTime(last.time)}`, text: "Windows is keeping this drive optimized on schedule." });
    }
  }
  const order = { danger: 0, warn: 1, info: 2, ok: 3 };
  out.sort((a, b) => order[a.tone] - order[b.tone]);
  if (!out.some((n) => n.tone === "danger" || n.tone === "warn")) {
    out.unshift({ tone: "ok", icon: "circle-check", title: "Nothing needs attention", text: `${driveName(v)} has plenty of room and no warnings.` });
  }
  return out;
}

function notesHTML(v) {
  return suggestions(v).map((n) => `
    <div class="note is-${n.tone}">${icon(n.icon)}
      <span class="note-title">${esc(n.title)}</span>
      ${n.action ? `<button class="btn sm" data-action="${n.action[1]}">${esc(n.action[0])}</button>` : ""}
      <span class="note-text">${esc(n.text)}</span>
    </div>`).join("");
}

function tabOverview(v) {
  const notes = notesHTML(v);
  const m = S.maps[v.letter]?.data;
  const used = v.total - v.free;
  const disk = diskFor(v.letter);
  return `<section class="map" id="map" aria-label="Allocation map"></section>
  <div class="grid-2">
    <div class="stack">
      <section class="panel"><h2 class="section-title">Suggestions</h2><p class="section-sub">Based on space, health and the last time Windows optimized this drive.</p><div class="notes" id="notes">${notes}</div></section>
      <section class="panel"><h2 class="section-title">Details</h2>
        <dl class="facts-list" style="margin-top:12px">
          <dt>Name</dt><dd>${esc(v.label || "No name")}</dd>
          <dt>Type</dt><dd>${esc(mediaLabel(v))}${v.model ? ` <span class="muted">(${esc(v.model)})</span>` : ""}</dd>
          <dt>File system</dt><dd>${esc(v.fileSystem || "Unknown")}</dd>
          <dt>Capacity</dt><dd>${fmtBytes(v.total)} <span class="muted">${fmtBytes(v.total, { exact: true })}</span></dd>
          <dt>Used</dt><dd>${fmtBytes(used)} <span class="muted">${fmtBytes(used, { exact: true })}</span></dd>
          <dt>Free</dt><dd>${fmtBytes(v.free)} <span class="muted">${fmtBytes(v.free, { exact: true })}</span></dd>
          <dt>Cluster size</dt><dd>${v.clusterSize ? fmtBytes(v.clusterSize) : "Unknown"}</dd>
          ${m ? `<dt>Free space pieces</dt><dd>${fmtInt(m.freeRuns)} <span class="muted">largest ${fmtBytes(m.largestFreeRun)}</span></dd>` : ""}
          <dt>Serial number</dt><dd>${esc(v.serial || "Unknown")}</dd>
        </dl>
      </section>
    </div>
    <div class="stack" id="health">${healthPanel(disk, v)}</div>
  </div>`;
}

// Background data (disks, history, clean-up sizes) arrived: on Overview, update only the parts that
// use it, so the allocation map and anything being hovered stay put.
function refreshOverviewBits() {
  if (S.route.view !== "drive" || S.route.tab !== "overview") return;
  const v = vol(S.route.letter); if (!v) return;
  const n = $("#notes"), h = $("#health");
  if (!n || !h) return renderPanel();
  restoringFocus(() => { patch(n, notesHTML(v)); patch(h, healthPanel(diskFor(v.letter), v)); });
}

function healthChip(h) {
  const tone = h === "Healthy" ? "is-pine" : h === "Warning" ? "is-warn" : h === "Unhealthy" ? "is-danger" : "";
  return `<span class="chip ${tone}">${icon(h === "Healthy" ? "circle-check" : "circle-alert")}${esc(h || "Unknown")}</span>`;
}

function healthPanel(disk, v) {
  if (S.disksLoading && !S.disks) return `<section class="panel"><h2 class="section-title">Disk health</h2><p class="section-sub">${icon("loader-circle", "spin")} Asking Windows about the disk…</p></section>`;
  if (!disk) return `<section class="panel"><h2 class="section-title">Disk health</h2><p class="section-sub">${S.disksError ? esc(S.disksError) : "Windows doesn't report health for this drive."}</p></section>`;
  return `<section class="panel">
    <div class="disk-head"><div><h2 class="section-title">Disk health</h2><p class="section-sub">${esc(disk.model)}, ${fmtBytes(disk.size)}</p></div>${healthChip(disk.health)}</div>
    ${metricsHTML(disk)}
  </section>`;
}

function metricsHTML(disk) {
  if (!disk.reliabilityAvailable) {
    return `<div class="admin-callout"><div>${icon("lock")}<span>Temperature, wear and error counts need administrator rights.</span></div>${S.sys.elevated ? "" : `<button class="btn sm" data-action="elevate">Restart as admin</button>`}</div>`;
  }
  const days = disk.powerOnHours != null ? Math.round(disk.powerOnHours / 24) : null;
  const errs = (disk.readErrors || 0) + (disk.writeErrors || 0);
  return `<div class="metrics" style="margin-top:6px">
    ${disk.temperature != null ? `<div><div class="metric-label">${icon("thermometer")}Temperature</div><div class="metric-value">${disk.temperature}<small>°C</small></div>${disk.temperatureMax ? `<div class="muted" style="font-size:var(--t-xs)">Limit ${disk.temperatureMax} °C</div>` : ""}</div>` : ""}
    ${disk.wear != null ? `<div><div class="metric-label">${icon("activity")}Life used</div><div class="metric-value">${disk.wear}<small>%</small></div></div>` : ""}
    ${disk.powerOnHours != null ? `<div><div class="metric-label">${icon("clock")}Powered on</div><div class="metric-value">${fmtInt(disk.powerOnHours)}<small>h</small></div><div class="muted" style="font-size:var(--t-xs)">About ${fmtInt(days)} days</div></div>` : ""}
    <div><div class="metric-label">${icon(errs ? "circle-alert" : "circle-check")}Read/write errors</div><div class="metric-value">${fmtInt(errs)}</div></div>
  </div>`;
}

/* ---------- space tab ---------- */

function tabSpace(v) {
  const sc = S.scan;
  const mine = sc.letter === v.letter;
  if (mine && sc.status === "scanning") {
    const p = sc.progress || {};
    return `<div class="progress-block" aria-live="polite">
      <div class="run-head"><h2 class="section-title">Measuring ${esc(sc.path)}</h2><button class="btn" data-action="scan-cancel">${icon("x")}Stop</button></div>
      <div class="progress-figs">
        <div class="fig"><b id="sp-bytes">${fmtBytes(p.bytes || 0)}</b><span>found so far</span></div>
        <div class="fig"><b id="sp-files">${fmtInt(p.files || 0)}</b><span>files</span></div>
        <div class="fig"><b id="sp-dirs">${fmtInt(p.dirs || 0)}</b><span>folders</span></div>
      </div>
      <div class="bar ${v.total && sc.isRoot ? "" : "is-indeterminate"}"><span id="sp-bar" style="width:${sc.isRoot ? Math.min(100, pct(p.bytes || 0, v.total - v.free)) : 30}%"></span></div>
      <div class="current-path" id="sp-current">${esc(p.current || "")}</div>
    </div>`;
  }
  if (!mine || sc.status === "idle" || sc.status === "error" || (sc.status === "done" && !sc.node)) {
    const other = sc.status === "done" && !mine && sc.letter ? `<p class="muted" style="margin:0 0 12px">Platter keeps one scan at a time. Scanning here replaces the scan of ${esc(sc.path)}.</p>` : "";
    return `<div class="empty">
      ${icon("scan-search")}
      <h2 class="h3">See what's taking up space on ${esc(v.letter)}:</h2>
      <p>Platter reads every folder on the drive and maps them by size, so the biggest ones stand out. Nothing is changed or uploaded.</p>
      ${mine && sc.status === "error" ? `<p style="color:var(--danger)">${esc(sc.error)}</p>` : ""}
      ${other}
      <div class="actions">
        <button class="btn primary lg" data-action="scan" data-path="${esc(v.letter)}:\\">${icon("scan-search")}Scan ${esc(v.letter)}:</button>
        <button class="btn lg" data-action="scan-folder">${icon("folder")}Scan a folder…</button>
      </div>
    </div>`;
  }
  // done
  const node = sc.node;
  const crumbs = sc.trail.map((c, i) => `${i ? icon("chevron-right", "sep") : ""}<button data-action="crumb" data-i="${i}" ${i === sc.trail.length - 1 ? 'aria-current="location"' : ""} title="${esc(c.path)}">${esc(i === 0 && sc.foreign ? sc.path : c.name)}</button>`).join("");
  const catsTotal = node.cats.reduce((a, b) => a + b, 0) || 1;
  const catOrder = node.cats.map((b, i) => [i, b]).filter(([, b]) => b > 0).sort((a, b) => b[1] - a[1]);
  const typebar = catOrder.map(([i, b]) => `<span style="width:${(b / catsTotal) * 100}%;background:${catVar(i)}" title="${CATS[i]}: ${fmtBytes(b)}"></span>`).join("");
  const legend = catOrder.map(([i, b]) => `<span><i class="swatch" style="background:${catVar(i)}"></i>${CATS[i]}<b>${fmtBytes(b)}</b></span>`).join("");
  const done = sc.done || {};
  const usedOnDrive = v.total - v.free;
  const gap = usedOnDrive - (done.totalBytes || 0);
  const notes = [];
  if (sc.isRoot && gap > usedOnDrive * 0.03) notes.push(`${icon("info")}Scanned ${fmtBytes(done.totalBytes)} of the ${fmtBytes(usedOnDrive)} in use. The other ${fmtBytes(gap)} is space Windows keeps for itself: restore points, file-system records and folders it protects.`);
  if (done.deniedDirs > 0) notes.push(`${icon("lock")}${fmtInt(done.deniedDirs)} folders couldn't be read${S.sys.elevated ? "." : `. <button class="link-btn" data-action="elevate">Restart as administrator</button> to include them.`}`);
  notes.push(`${icon("clock")}Scanned ${fmtInt(done.files)} files in ${((done.elapsedMs || 0) / 1000).toFixed(1)} seconds.`);

  const body = sc.view === "files" ? filesTable() : `
    <div class="space-body">
      <div><div class="treemap" id="treemap"></div><div class="treemap-status" id="tm-status">${statusFor(null)}</div></div>
      <div class="rows" id="rows">${rowsHTML(node)}</div>
    </div>`;

  return `
    <div class="space-toolbar">
      <div class="crumbs">
        <button data-action="crumb-up" title="Up one level" aria-label="Up one level" ${sc.trail.length < 2 ? "disabled" : ""}>${icon("arrow-up")}</button>
        ${crumbs}
      </div>
      <div style="display:flex;gap:8px;align-items:center">
        <div class="seg" role="group" aria-label="View">
          <button data-action="space-view" data-v="map" aria-pressed="${sc.view !== "files"}">${icon("layers")}Folders</button>
          <button data-action="space-view" data-v="files" aria-pressed="${sc.view === "files"}">${icon("list")}Largest files</button>
        </div>
        <button class="btn icon" data-action="scan" data-path="${esc(sc.path)}" title="Scan again" aria-label="Scan again">${icon("refresh-cw")}</button>
        <button class="btn icon" data-action="scan-folder" title="Scan a different folder" aria-label="Scan a different folder">${icon("folder")}</button>
      </div>
    </div>
    ${sc.view === "files" ? "" : `<div class="typebar" aria-hidden="true">${typebar}</div><div class="type-legend">${legend}</div>`}
    ${body}
    <div class="space-foot">${notes.map((n) => `<div>${n}</div>`).join("")}</div>`;
}

function statusFor(c) {
  const node = S.scan.node;
  if (!c) return `<b>${esc(node.name)}</b>: ${fmtBytes(node.size)} in ${fmtInt(node.files)} files and ${fmtInt(node.dirs)} folders. Click a folder to open it.`;
  if (c.isOther) return `<b>${esc(c.name)}</b>: ${fmtBytes(c.size)} together, each too small to show.`;
  return `<b>${esc(c.name)}</b>: ${fmtBytes(c.size)}, ${pct(c.size, node.size)}% of ${esc(node.name)}${c.isDir ? `, ${fmtInt(c.files)} files` : ""}. Mostly ${CATS[c.cat ?? 7].toLowerCase()}.`;
}

function visibleChildren(node) {
  return (node.children || []).filter((c) => c.size > 0);
}

function rowsHTML(node) {
  const kids = visibleChildren(node);
  if (!kids.length) return `<div class="map-empty">${icon("folder")}This folder is empty.</div>`;
  const max = kids[0].size || 1;
  return kids.map((c, i) => `
    <div class="row ${c.isOther ? "is-other" : ""}" data-k="${i}" ${c.isOther ? "" : `data-action="${c.isDir ? "drill" : "reveal"}"`} ${c.isDir ? `data-id="${c.id}"` : ""} data-path="${esc(c.path)}" tabindex="${c.isOther ? -1 : 0}">
      ${icon(c.isDir ? "folder" : c.isOther ? "layers" : "file")}
      <span class="row-name" title="${esc(c.path || c.name)}">${esc(c.name)}${c.denied ? ` ${icon("lock")}` : ""}</span>
      <span class="row-size">${fmtBytes(c.size)}</span>
      <div class="row-bar"><span style="width:${Math.max(0.5, (c.size / max) * 100)}%;background:${catVar(c.cat)}"></span></div>
      ${c.isOther ? "" : `<div class="row-actions">
        <button class="btn sm icon ghost" data-action="${c.isDir ? "open-path" : "reveal"}" data-path="${esc(c.path)}" title="${c.isDir ? "Open in Explorer" : "Show in Explorer"}" aria-label="${c.isDir ? "Open in Explorer" : "Show in Explorer"}">${icon("folder-open")}</button>
        <button class="btn sm icon ghost" data-action="recycle" data-path="${esc(c.path)}" data-name="${esc(c.name)}" title="Move to Recycle Bin" aria-label="Move ${esc(c.name)} to Recycle Bin">${icon("trash-2")}</button>
      </div>`}
    </div>`).join("");
}

function filesTable() {
  if (!S.scan.topFiles) { loadTopFiles(); return `<div class="map-empty">${icon("loader-circle", "spin")}Finding the largest files…</div>`; }
  const files = S.scan.topFiles.filter((f) => !isRemoved(f.path));
  if (!files.length) return `<div class="empty"><h2 class="h3">No files found</h2><p>This folder has no files.</p></div>`;
  return `<table class="files-table"><thead><tr><th>File</th><th>Type</th><th class="r">Size</th><th><span class="sr-only">Actions</span></th></tr></thead><tbody>
    ${files.map((f) => {
      const dir = f.path.slice(0, f.path.length - f.name.length - 1);
      return `<tr>
        <td><span class="file-name">${esc(f.name)}</span><span class="file-dir" title="${esc(f.path)}">${esc(dir)}</span></td>
        <td><span class="cat-dot" style="background:${catVar(f.cat)}"></span>${CATS[f.cat ?? 7]}</td>
        <td class="r"><b>${fmtBytes(f.size)}</b></td>
        <td class="td-actions">
          <button class="btn sm icon ghost" data-action="reveal" data-path="${esc(f.path)}" title="Show in Explorer" aria-label="Show ${esc(f.name)} in Explorer">${icon("folder-open")}</button>
          <button class="btn sm icon ghost" data-action="recycle" data-path="${esc(f.path)}" data-name="${esc(f.name)}" title="Move to Recycle Bin" aria-label="Move ${esc(f.name)} to Recycle Bin">${icon("trash-2")}</button>
        </td></tr>`;
    }).join("")}</tbody></table>`;
}

async function loadTopFiles() {
  const sc = S.scan;
  if (sc.loadingTop) return;
  sc.loadingTop = true;
  let files;
  try { files = await invoke("scan_top_files", { limit: 100, ...scanIdArg(sc) }); } catch (e) { files = []; toast(errText(e), { tone: "bad" }); }
  sc.loadingTop = false;
  if (S.scan !== sc) return;
  sc.topFiles = files;
  if (S.route.view === "drive" && S.route.tab === "space") renderPanel();
}

/* squarified treemap */
function squarify(values, x, y, w, h) {
  const total = values.reduce((a, b) => a + b, 0);
  const rects = new Array(values.length);
  if (total <= 0 || w <= 0 || h <= 0) return rects;
  const scale = (w * h) / total;
  const areas = values.map((v) => v * scale);
  let r = { x, y, w, h };
  let row = [];
  const worst = (row, side) => {
    const s = row.reduce((a, b) => a + areas[b], 0);
    let mx = 0, mn = Infinity;
    for (const i of row) { mx = Math.max(mx, areas[i]); mn = Math.min(mn, areas[i]); }
    return Math.max((side * side * mx) / (s * s), (s * s) / (side * side * mn));
  };
  const place = (row) => {
    const s = row.reduce((a, b) => a + areas[b], 0);
    if (r.w >= r.h) {
      const cw = s / r.h; let yy = r.y;
      for (const i of row) { const hh = areas[i] / cw; rects[i] = { x: r.x, y: yy, w: cw, h: hh }; yy += hh; }
      r = { x: r.x + cw, y: r.y, w: r.w - cw, h: r.h };
    } else {
      const rh = s / r.w; let xx = r.x;
      for (const i of row) { const ww = areas[i] / rh; rects[i] = { x: xx, y: r.y, w: ww, h: rh }; xx += ww; }
      r = { x: r.x, y: r.y + rh, w: r.w, h: r.h - rh };
    }
  };
  let i = 0;
  while (i < areas.length) {
    const side = Math.min(r.w, r.h);
    if (!row.length || worst([...row, i], side) <= worst(row, side)) { row.push(i); i++; }
    else { place(row); row = []; }
  }
  if (row.length) place(row);
  return rects;
}

function drawTreemap() {
  const host = $("#treemap"); const node = S.scan.node;
  if (!host || !node) return;
  const W = host.clientWidth, H = host.clientHeight;
  const kids = visibleChildren(node);
  const rects = squarify(kids.map((c) => c.size), 0, 0, W, H);
  let html = "";
  kids.forEach((c, k) => {
    const r = rects[k]; if (!r || r.w < 2 || r.h < 2) return;
    const big = r.w > 64 && r.h > 34;
    const nest = c.isDir && c.children?.length && r.w > 150 && r.h > 96;
    const bg = nest ? `color-mix(in oklch, ${catVar(c.cat)}, var(--ink) 16%)` : catVar(c.cat);
    let inner = "";
    if (nest) {
      const gk = visibleChildren(c);
      const iw = r.w - 8, ih = r.h - 28;
      const gr = squarify(gk.map((g) => g.size), 0, 0, iw, ih);
      inner = `<div class="inner">${gk.map((g, j) => {
        const q = gr[j]; if (!q || q.w < 3 || q.h < 3) return "";
        const lbl = q.w > 70 && q.h > 30;
        return `<div class="tile is-sub ${g.isOther ? "is-other" : ""}" style="left:${q.x}px;top:${q.y}px;width:${q.w}px;height:${q.h}px" title="${esc(g.name)}: ${fmtBytes(g.size)}"><div class="tile-in" style="background:${catVar(g.cat)}"></div>${lbl ? `<div class="tile-label"><b>${esc(g.name)}</b><span>${fmtBytes(g.size)}</span></div>` : ""}</div>`;
      }).join("")}</div>`;
    }
    html += `<div class="tile ${c.isOther ? "is-other" : ""}" data-k="${k}" ${c.isOther ? "" : `data-action="${c.isDir ? "drill" : "reveal"}"`} ${c.isDir ? `data-id="${c.id}"` : ""} data-path="${esc(c.path)}"
      style="left:${r.x}px;top:${r.y}px;width:${r.w}px;height:${r.h}px">
      <div class="tile-in" style="background:${bg};${nest ? "color:var(--cat-ink)" : ""}"></div>
      ${big ? `<div class="tile-label"><b>${esc(c.name)}</b><span>${fmtBytes(c.size)}</span></div>` : ""}${inner}</div>`;
  });
  lastTreemapSize = `${W}x${H}`;
  // The rows aren't rebuilt here: clear their highlight too, or it would stay stuck after a resize.
  hotEls.forEach((e) => e.classList.remove("is-hot"));
  hotEls = []; hotK = null;
  host.innerHTML = html;
}

// Hover highlight links a treemap tile with its row. Moving within one tile (over its label or inner
// tiles) fires many mouseovers: only act when the hovered item actually changes.
let hotK = null, hotEls = [];
function hot(k) {
  const st = $("#tm-status");
  if (k === hotK && hotEls.every((e) => e.isConnected)) return;
  hotEls.forEach((e) => e.classList.remove("is-hot"));
  hotK = k; hotEls = [];
  if (k == null) { if (st) patch(st, statusFor(null)); return; }
  for (const e of [$(`#treemap > .tile[data-k="${k}"]`), $(`#rows .row[data-k="${k}"]`)]) if (e) { e.classList.add("is-hot"); hotEls.push(e); }
  const c = visibleChildren(S.scan.node)[k];
  if (st && c) patch(st, statusFor(c));
}
document.addEventListener("mouseover", (e) => {
  if (hotK == null && !e.target.closest?.("#treemap, #rows")) return;
  const t = e.target.closest("#treemap > .tile, #rows .row");
  if (t) hot(t.dataset.k); else if (e.target.closest("#treemap, #rows") === null) hot(null);
});

// The drive page a scan is shown on. A UNC path (\\server\share) or a folder on a drive Platter
// doesn't list has no page of its own, so it is shown on the drive page the user is on.
function scanHostLetter(path) {
  const m = /^([A-Za-z]):/.exec(path);
  if (m && vol(m[1].toUpperCase())) return m[1].toUpperCase();
  if (S.route.view === "drive" && vol(S.route.letter)) return S.route.letter;
  return (vol(S.sys.systemDrive) || S.volumes[0])?.letter;
}

async function startScan(path) {
  path = String(path || "").replace(/[\\/]+$/, "");
  if (/^[A-Za-z]:$/.test(path)) path += "\\";
  if (!path) return;
  const letter = scanHostLetter(path);
  if (!letter) return toast("Platter needs at least one drive to show a scan on.", { tone: "bad" });
  const onDrive = /^[A-Za-z]:/.test(path) && path[0].toUpperCase() === letter;
  const isRoot = onDrive && /^[A-Za-z]:\\$/.test(path);
  // id: the backend's scan id once scan_start returns; events that arrive before then wait in `pending`.
  const sc = { status: "scanning", path, letter, isRoot, foreign: !onDrive, progress: { files: 0, dirs: 0, bytes: 0, current: path }, view: "map",
    id: undefined, pending: [], removed: new Map(), openSeq: 0 };
  S.scan = sc;
  if (!(S.route.view === "drive" && S.route.letter === letter && S.route.tab === "space")) go({ view: "drive", letter, tab: "space" });
  else renderPanel();
  try {
    const id = await invoke("scan_start", { path });
    if (S.scan !== sc) return;
    sc.id = typeof id === "number" ? id : null;
    const queued = sc.pending; sc.pending = null;
    for (const [kind, d] of queued) (kind === "done" ? onScanDone : onScanProgress)(d);
  } catch (e) {
    if (S.scan !== sc) return;
    S.scan = { status: "error", letter, path, error: errText(e) };
    if (S.route.view === "drive" && S.route.tab === "space") renderPanel();
  }
}

// Is this scan event for the scan the UI started? Events carry `scanId` (the value scan_start
// returned); a late event from a stopped or replaced scan is dropped. Without scanId, accept it.
function scanEventIsMine(kind, d) {
  const sc = S.scan;
  if (sc.status !== "scanning") return false;
  if (d?.scanId == null) return true;
  if (sc.pending) { sc.pending.push([kind, d]); return false; }
  return sc.id == null || d.scanId === sc.id;
}

// scan_node / scan_top_files take the scan id so ids from a replaced scan can't resolve against a newer one.
const scanIdArg = (sc) => (typeof sc.id === "number" ? { scanId: sc.id } : {});

async function openNode(id, trailIndex) {
  const sc = S.scan;
  if (!sc.trail) return;
  const seq = ++sc.openSeq;
  sc.opening = true;
  try {
    const raw = await invoke("scan_node", { id, depth: 2, ...scanIdArg(sc) });
    if (S.scan !== sc || seq !== sc.openSeq) return; // a newer navigation or scan won
    sc.raw = raw;
    sc.node = pruneView(raw);
    if (trailIndex != null) sc.trail = sc.trail.slice(0, trailIndex + 1);
    else if (sc.trail[sc.trail.length - 1]?.id !== id) sc.trail.push({ id, name: raw.name, path: raw.path });
    if (S.route.view === "drive" && S.route.tab === "space") renderPanel();
  } catch (e) {
    if (S.scan === sc) toast(errText(e), { tone: "bad" });
  } finally {
    if (seq === sc.openSeq) sc.opening = false;
  }
}

/* Items recycled from the scan. The backend keeps the scan as it was, so every node fetched later
   is corrected here: removed children are dropped and their bytes, files and folders are taken off
   every folder that contained them (per category, so the type bar stays right). */
const lowerPath = (p) => String(p ?? "").toLowerCase();
function isUnder(p, dir) {
  if (!dir) return false;
  const base = dir.endsWith("\\") ? dir : `${dir}\\`;
  return p.length > base.length && p.startsWith(base);
}
const parentPath = (p) => { const i = p.lastIndexOf("\\"); return i < 0 ? "" : p.slice(0, i + (p[i - 1] === ":" ? 1 : 0)); };
function catsFor(cat, size) { const c = [0, 0, 0, 0, 0, 0, 0, 0]; c[cat ?? 7] = size; return c; }
function isRemoved(path) {
  const rm = S.scan.removed; if (!rm?.size) return false;
  const p = lowerPath(path);
  if (rm.has(p)) return true;
  for (const [rp, r] of rm) if (r.isDir && isUnder(p, rp)) return true;
  return false;
}
function subtractFrom(n, r) {
  n.size = Math.max(0, (n.size || 0) - r.size);
  if (Array.isArray(n.cats)) n.cats = n.cats.map((b, i) => Math.max(0, b - (r.cats[i] || 0)));
  n.files = Math.max(0, (n.files || 0) - r.files);
  n.dirs = Math.max(0, (n.dirs || 0) - r.dirs);
}
function pruneView(raw) {
  const rm = S.scan.removed;
  if (!raw || !rm?.size) return raw;
  const node = structuredClone(raw);
  const fix = (n) => {
    const np = lowerPath(n.path);
    if (n.isDir && np) for (const [rp, r] of rm) if (isUnder(rp, np)) subtractFrom(n, r);
    if (!n.children?.length) return;
    const listed = new Set(n.children.filter((c) => !c.isOther).map((c) => lowerPath(c.path)));
    const other = n.children.find((c) => c.isOther);
    // A recycled file that wasn't listed on its own sat in this folder's "smaller files" bucket.
    if (other && n.isDir) for (const [rp, r] of rm) if (!r.isDir && !listed.has(rp) && parentPath(rp) === np) subtractFrom(other, r);
    n.children = n.children.filter((c) => c.isOther || !rm.has(lowerPath(c.path)));
    n.children.forEach(fix);
    n.children.sort((a, b) => b.size - a.size);
  };
  fix(node);
  return node;
}

function removeFromScan(path) {
  const sc = S.scan; if (sc.status !== "done" || !sc.removed) return;
  const p = lowerPath(path);
  if (!isRemoved(path)) {
    const c = (sc.node?.children || []).find((x) => !x.isOther && lowerPath(x.path) === p);
    const f = !c && (sc.topFiles || []).find((x) => lowerPath(x.path) === p);
    // Sizes recorded here are the corrected ones shown on screen, so nested removals add up exactly.
    const entry = c ? { size: c.size, cats: Array.isArray(c.cats) ? [...c.cats] : catsFor(c.cat, c.size), files: c.isDir ? c.files || 0 : 1, dirs: c.isDir ? (c.dirs || 0) + 1 : 0, isDir: !!c.isDir }
      : f ? { size: f.size, cats: catsFor(f.cat, f.size), files: 1, dirs: 0, isDir: false }
      : { size: 0, cats: catsFor(7, 0), files: 0, dirs: 0, isDir: false };
    sc.removed.set(p, entry);
  }
  if (sc.raw) sc.node = pruneView(sc.raw);
  if (S.route.view === "drive" && S.route.tab === "space") renderPanel();
}

/* ---------- optimize tab ---------- */

function tabOptimize(v) {
  const fsOk = ["NTFS", "ReFS", "FAT32", "FAT"].includes(v.fileSystem);
  if (!v.ready || !fsOk || v.kind === "network" || v.kind === "optical") {
    return `<div class="empty">${icon("info")}<h2 class="h3">Windows can't optimize this drive</h2><p>${v.kind === "network" ? "Network drives are optimized by the computer that hosts them." : `Optimization works on NTFS, ReFS and FAT32 drives. This drive uses ${esc(v.fileSystem || "an unknown file system")}.`}</p></div>`;
  }
  const isSsd = v.media === "ssd";
  const lockIc = S.sys.elevated ? "" : icon("lock");
  const run = S.opt.running?.letter === v.letter ? S.opt.running : null;
  const busy = !!S.opt.running;
  const res = S.opt.results[v.letter];
  let reco;
  if (isSsd) {
    reco = `<div class="reco"><h2 class="section-title">Trim keeps this SSD fast</h2>
      <div class="reco-actions"><button class="btn primary lg" data-action="optimize" data-mode="trim" ${busy ? "disabled" : ""}>${lockIc || icon("zap")}Trim ${esc(v.letter)}:</button></div>
      <p class="section-sub">Trimming tells the SSD which blocks no longer hold data, so it can clear them ahead of time and keep writes quick. Defragmenting isn't offered for SSDs: it adds wear without making them faster.</p></div>`;
  } else {
    reco = `<div class="reco"><h2 class="section-title">${v.media === "hdd" ? "Defragment to speed up this hard disk" : "Optimize this drive"}</h2>
      <div class="reco-actions">
        <button class="btn lg" data-action="optimize" data-mode="analyze" ${busy ? "disabled" : ""}>${lockIc || icon("search")}Analyze</button>
        <button class="btn primary lg" data-action="optimize" data-mode="defrag" ${busy ? "disabled" : ""}>${lockIc || icon("gauge")}Defragment ${esc(v.letter)}:</button>
      </div>
      <p class="section-sub">Files get split into pieces scattered across the disk over time, and reading them gets slower. Defragmenting puts the pieces back together. Analyze first to see whether it's worth the time.</p></div>`;
  }
  const runHTML = run ? `<div class="run" aria-live="polite">
      <div class="run-head"><span><b id="op-stage">${esc(run.stage || "Starting")}</b> <span class="muted" id="op-pct">${run.percent != null ? `${Math.round(run.percent)}%` : ""}</span></span><button class="btn sm" data-action="optimize-cancel">${icon("x")}Stop</button></div>
      <div class="bar ${run.percent == null ? "is-indeterminate" : ""}" id="op-bar-wrap"><span id="op-bar" style="width:${run.percent ?? 0}%"></span></div>
      <div class="run-line" id="op-line">${esc(run.line || "")}</div></div>` : "";
  const resHTML = !run && res ? `<div class="result ${res.ok ? "is-ok" : "is-bad"}">${icon(res.ok ? "circle-check" : "circle-alert")}<div><b>${esc(res.message)}</b>${res.fragmentedPercent != null ? `<div class="muted">${res.fragmentedPercent < 10 ? "That's low; defragmenting now wouldn't make a noticeable difference." : "That's high enough that defragmenting should help."}</div>` : ""}</div></div>` : "";
  const adminNote = S.sys.elevated ? "" : `<div class="admin-callout"><div>${icon("lock")}<span>Windows only lets administrators ${isSsd ? "trim" : "defragment"} drives.</span></div><button class="btn sm" data-action="elevate">Restart as administrator</button></div>`;

  const hist = (S.history || []).filter((h) => h.letter === v.letter);
  const histHTML = S.history == null ? `<p class="section-sub">${icon("loader-circle", "spin")} Reading Windows' optimization log…</p>`
    : !hist.length ? `<p class="section-sub">Windows hasn't logged any optimization of ${esc(v.letter)}: yet.</p>`
    : `<ul class="history">${hist.slice(0, 8).map((h) => `<li class="${h.ok ? "" : "is-bad"}">${icon(h.ok ? "circle-check" : "circle-alert")}<span>${esc(h.action)}${h.ok ? "" : ` <span class="muted">failed</span>`}</span><time datetime="${esc(h.time)}" title="${esc(new Date(h.time).toLocaleString())}">${shortDate(h.time)}, ${relTime(h.time)}</time></li>`).join("")}</ul>`;
  const sch = S.schedule;
  const schHTML = !sch ? (S.historyLoading ? `<p class="section-sub">${icon("loader-circle", "spin")} Reading Windows' optimization schedule…</p>` : `<p class="section-sub">Couldn't read Windows' optimization schedule.</p>`)
    : `<p class="section-sub" style="margin-bottom:10px">${sch.enabled ? "Windows optimizes your drives automatically about once a week, while the PC is idle." : "Windows' automatic weekly optimization is turned off. Turn it on from the Optimize Drives app in Windows."}</p>
       <dl class="facts-list">
         <dt>Automatic</dt><dd>${sch.enabled ? `<span class="chip is-pine">${icon("check")}On</span>` : `<span class="chip is-warn">Off</span>`}</dd>
         ${sch.lastRun ? `<dt>Last run</dt><dd>${shortDate(sch.lastRun)} <span class="muted">(${relTime(sch.lastRun)})</span></dd>` : ""}
         ${sch.nextRun && sch.enabled ? `<dt>Next run</dt><dd>${shortDate(sch.nextRun)} <span class="muted">(${relTime(sch.nextRun)})</span></dd>` : sch.enabled && !sch.nextRun ? `<dt>Next run</dt><dd>Next time the PC is idle during maintenance</dd>` : ""}
       </dl>`;
  return `<div class="stack">
    <section class="panel">${reco}${runHTML}${resHTML}${adminNote}</section>
    <div class="grid-2">
      <section class="panel"><h2 class="section-title">History</h2><p class="section-sub">From Windows' own optimization log for ${esc(v.letter)}:.</p>${histHTML}</section>
      <section class="panel"><h2 class="section-title">Schedule</h2>${schHTML}</section>
    </div></div>`;
}

async function startOptimize(letter, mode) {
  if (!S.sys.elevated) return askElevate(mode === "trim" ? "Windows only lets administrators trim drives." : "Windows only lets administrators analyze and defragment drives.");
  S.opt.running = { letter, mode, percent: null, stage: mode === "trim" ? "Trim" : mode === "defrag" ? "Defragmentation" : "Analysis", line: "Starting…" };
  delete S.opt.results[letter];
  renderPanel();
  try { await invoke("optimize_start", { letter, mode }); }
  catch (e) { S.opt.running = null; S.opt.results[letter] = { ok: false, message: errText(e) }; renderPanel(); }
}

/* ---------- manage tab ---------- */

function letterOptionsHTML(v) {
  const free = S.freeLetters;
  if (!free) return `<option disabled>…</option>`;
  if (!free.length) return `<option disabled>None free</option>`;
  const want = S.drafts.letter[v.letter];
  return free.map((l) => `<option ${want === `${l}:` ? "selected" : ""}>${l}:</option>`).join("");
}

function checkControlHTML(v) {
  const ck = S.checks[v.letter];
  const canNTFS = v.fileSystem === "NTFS" || v.fileSystem === "ReFS";
  // ErrorsFixed means Windows repaired small problems online: good news, not an error.
  const okStatus = ck && (ck.status === "NoErrorsFound" || ck.status === "ErrorsFixed");
  return `<button class="btn" data-action="check" ${ck?.status === "running" || !canNTFS ? "disabled" : ""}>${ck?.status === "running" ? icon("loader-circle", "spin") + "Checking…" : (S.sys.elevated ? icon("search") : icon("lock")) + `Check ${esc(v.letter)}:`}</button>
        ${!canNTFS ? `<span class="field-msg">Only NTFS and ReFS drives can be checked while in use.</span>` : ""}
        ${ck && ck.status !== "running" ? `<span class="field-msg ${ck.error ? "is-error" : okStatus ? "is-ok" : "is-error"}">${esc(ck.error || ck.message)}</span>` : ""}`;
}

function tabManage(v) {
  const draft = S.drafts.label[v.letter];
  const label = draft ?? v.label ?? "";
  const bad = LABEL_BAD_RE.test(label);
  return `<section class="panel"><div class="form-rows">
    <div class="form-row">
      <div><h2 class="h3">Name</h2><p class="section-sub">Shown in Explorer next to the drive letter. Up to 32 characters.</p></div>
      <form class="control" data-form="label">
        <input class="input" id="label-input" name="label" maxlength="32" value="${esc(label)}" placeholder="Local Disk" aria-label="Drive name" autocomplete="off" spellcheck="false" ${bad ? 'aria-invalid="true"' : ""} />
        <button class="btn primary" id="label-save" ${bad || label === (v.label || "") ? "disabled" : ""}>Rename</button>
        <span class="field-msg ${bad ? "is-error" : ""}" id="label-msg">${bad ? esc(LABEL_BAD) : ""}</span>
      </form>
    </div>
    <div class="form-row">
      <div><h2 class="h3">Drive letter</h2><p class="section-sub">Programs and shortcuts that point to ${esc(v.letter)}: will stop finding their files if the letter changes.</p></div>
      ${v.isSystem ? `<div class="control"><span class="field-msg">Windows is installed on ${esc(v.letter)}:, so its letter can't change.</span></div>`
        : `<form class="control" data-form="letter">
            <select class="select" id="letter-select" aria-label="New drive letter">${letterOptionsHTML(v)}</select>
            <button class="btn">${S.sys.elevated ? "" : icon("lock")}Change letter</button>
            <span class="field-msg" id="letter-msg"></span>
          </form>`}
    </div>
    <div class="form-row">
      <div><h2 class="h3">Check for errors</h2><p class="section-sub">Reads the file system and reports problems without changing anything. Takes a minute or two; you can keep using the PC.</p></div>
      <div class="control" id="check-ctl">${checkControlHTML(v)}</div>
    </div>
  </div></section>`;
}

/* ------------------------------------------------------------------ This PC */

function viewPC() {
  const disks = S.disks;
  let disksHTML;
  if (S.disksLoading && !disks) disksHTML = `<section class="panel"><p class="section-sub" style="margin:0">${icon("loader-circle", "spin")} Asking Windows about your disks…</p></section>`;
  else if (S.disksError && !disks) disksHTML = `<section class="panel"><h2 class="section-title">Couldn't read your disks</h2><p class="section-sub">${esc(S.disksError)}</p><button class="btn" data-action="reload-disks">${icon("refresh-cw")}Try again</button></section>`;
  else disksHTML = (disks || []).map(diskHTML).join("");
  const vols = S.volumes;
  return `<div class="page">
    <header class="page-head">
      <div><h1 class="page-title">This PC</h1><p class="page-sub">${esc(S.sys.computerName || "")}: ${disks ? `${disks.length} ${disks.length === 1 ? "disk" : "disks"}, ` : ""}${vols.length} ${vols.length === 1 ? "drive" : "drives"}.</p></div>
      <div class="head-actions"><button class="btn" data-action="rescan-disks" title="Look again for disks Windows hasn't picked up, like one you just connected">${icon("refresh-cw")}Rescan disks</button></div>
    </header>
    <div class="stack">
      ${disksHTML}
      <section class="panel"><h2 class="section-title">Drives</h2>
        <table class="vol-table" style="margin-top:12px"><thead><tr><th>Drive</th><th>Type</th><th>Used</th><th class="r">Free</th><th class="r">Size</th></tr></thead><tbody>
        ${vols.map((v) => `<tr class="is-link" data-action="go-drive" data-letter="${v.letter}" tabindex="0">
          <td><span class="vol-name">${icon(driveIcon(v))}${esc(driveName(v))}</span></td>
          <td class="muted">${esc(mediaLabel(v))}${v.fileSystem ? `, ${esc(v.fileSystem)}` : ""}</td>
          <td><div class="vol-meter"><div class="meter ${fullTone(v)}"><span style="width:${(fullness(v) * 100).toFixed(1)}%"></span></div></div></td>
          <td class="r">${v.ready ? fmtBytes(v.free) : notReadyShort(v)}</td>
          <td class="r">${v.ready ? fmtBytes(v.total) : ""}</td></tr>`).join("")}
        </tbody></table>
      </section>
    </div></div>`;
}

function partLabel(p) {
  if (p.letter) { const v = vol(p.letter); return v ? driveName(v) : `${p.letter}:`; }
  return { "EFI system": "EFI", "Microsoft reserved": "Reserved", Recovery: "Recovery" }[p.kind] || p.kind || "Partition";
}

function diskHTML(d) {
  const total = d.size || 1;
  const parts = (d.partitions || []).filter((p) => p.kind !== "Extended"); // logical drives are listed on their own
  const segs = parts.map((p) => {
    const v = p.letter ? vol(p.letter) : null;
    const used = v && v.total ? (v.total - v.free) / v.total : null;
    const grow = Math.max(p.size / total, 0.001);
    const tiny = !v && grow < 0.04;
    return `<${v ? "button" : "div"} class="part ${v ? "is-volume" : ""} ${tiny ? "is-tiny" : ""}" style="${tiny ? "" : `flex:${grow} 1 0`}" ${v ? `data-action="go-drive" data-letter="${v.letter}"` : ""} title="${esc(partLabel(p))}: ${esc(p.kind)}, ${fmtBytes(p.size)}">
      ${tiny ? `<span class="sr-only">${esc(partLabel(p))}, ${fmtBytes(p.size)}</span>` : `<b>${esc(partLabel(p))}</b><span>${fmtBytes(p.size)}</span>`}${used != null ? `<i class="fill" style="width:${used * 100}%"></i>` : ""}
    </${v ? "button" : "div"}>`;
  });
  if (d.unallocated > 0) segs.push(`<div class="part is-unalloc" style="flex:${d.unallocated / total} 1 0" title="Unallocated: ${fmtBytes(d.unallocated)}"><b>Unallocated</b><span>${fmtBytes(d.unallocated)}</span></div>`);
  const hidden = parts.filter((p) => !p.letter);
  return `<section class="panel disk">
    <div class="disk-head">
      <div><h2 class="section-title">Disk ${d.number}: ${esc(d.model)}</h2>
        <div class="chips">
          <span class="chip">${fmtBytes(d.size)}</span>
          ${d.bus ? `<span class="chip">${esc(d.bus)}</span>` : ""}
          ${d.media !== "unknown" ? `<span class="chip">${d.media === "ssd" ? "SSD" : "Hard disk"}</span>` : ""}
          ${d.partitionStyle ? `<span class="chip">${esc(d.partitionStyle)}</span>` : ""}
          ${d.isBoot ? `<span class="chip is-pine">Starts Windows</span>` : ""}
        </div></div>
      ${healthChip(d.health)}
    </div>
    <div class="partmap" role="list" aria-label="Partitions on disk ${d.number}">${segs.join("")}</div>
    ${hidden.length ? `<p class="part-legend" style="margin:-6px 0 0">${hidden.length} hidden ${hidden.length === 1 ? "partition holds" : "partitions hold"} startup and recovery files (${fmtBytes(hidden.reduce((a, p) => a + p.size, 0))}). Windows needs them; leave them alone.</p>` : ""}
    ${metricsHTML(d)}
  </section>`;
}

/* ------------------------------------------------------------------ Clean up */

function selectable(it) {
  return it.available && it.kind !== "review" && it.size > 0 && !(it.needsAdmin && !S.sys.elevated);
}
function selectedTotal() {
  return S.cleanup.items.filter((it) => S.cleanup.selected.has(it.id) && selectable(it)).reduce((a, it) => a + it.size, 0);
}

const CLEAN_GROUPS = ["System", "Browsers", "Apps", "Developer caches", "Review"];

// The Clean up page is a fixed frame; updateCleanup() fills and refreshes its parts in place.
function viewCleanup() {
  const sub = cleanSub();
  return `<div class="page" id="clean-page" data-sub="${sub}">
    <header class="page-head">
      <div><h1 class="page-title">Clean up</h1><p class="page-sub">${esc(CLEAN_SUB_TEXT[sub])}</p></div>
      <div class="head-actions" id="clean-head"></div>
    </header>
    <nav class="tabs" role="tablist" aria-label="Clean up sections">
      ${CLEAN_SUBS.map(([k, label]) => `<button class="tab" role="tab" data-action="clean-sub" data-sub="${k}" aria-selected="${sub === k}">${label}</button>`).join("")}
    </nav>
    ${sub === "caches" ? `<section class="clean-summary" id="clean-summary"></section>
    <div class="contents" id="clean-notices"></div>
    <div class="groups" id="clean-groups"></div>
    <div class="contents" id="clean-foot"></div>` : `<div id="${sub}-body"></div>`}
  </div>`;
}

function cleanAgeText() {
  const C = S.cleanup;
  return C.measuredAt && C.status !== "analyzing" ? `Measured ${relTime(C.measuredAt)}` : "";
}

function updateCleanup() {
  const sub = cleanSub();
  if (sub === "apps") return updateApps();
  if (sub === "dupes") return updateDupes();
  updateCaches();
}

function updateCaches() {
  const C = S.cleanup;
  const items = C.items.filter((it) => it.available);
  const missing = C.items.filter((it) => !it.available && it.kind !== "review");
  const lockedItems = items.filter((it) => it.needsAdmin && !S.sys.elevated);
  const lockedSize = lockedItems.reduce((a, it) => a + it.size, 0);
  const running = C.status === "running";
  const analyzing = C.status === "analyzing";
  const age = cleanAgeText();

  patch($("#clean-head"), `${age ? `<span class="clean-age" id="clean-age">${esc(age)}</span>` : ""}<button class="btn" data-action="cleanup-analyze" ${analyzing || running ? "disabled" : ""}>${analyzing ? icon("loader-circle", "spin") + "Measuring…" : icon("refresh-cw") + "Measure again"}</button>`);
  patch($("#clean-summary"), cleanSummaryHTML());
  patch($("#clean-notices"), `${lockedItems.length ? `<div class="admin-callout" style="margin:-8px 0 24px"><div>${icon("lock")}<span>${esc(listNames(lockedItems.map((it) => it.name)))} ${lockedItems.length === 1 ? "needs" : "need"} administrator rights to measure and clean${lockedSize ? ` (at least ${fmtBytes(lockedSize)})` : ""}.</span></div><button class="btn sm" data-action="elevate">Restart as administrator</button></div>` : ""}
    ${C.lastResult ? resultBanner(C.lastResult) : ""}`);

  const host = $("#clean-groups");
  const present = CLEAN_GROUPS.filter((g) => items.some((it) => it.group === g));
  if (!present.length) {
    patch(host, analyzing ? "" : `<div class="empty"><h2 class="h3">Nothing measured yet</h2><p>Measure to see how much space caches and leftovers are using.</p><div class="actions"><button class="btn primary" data-action="cleanup-analyze">Measure</button></div></div>`);
  } else {
    host._h = undefined;
    // Group frames never change; their size and items are updated inside them, item by item.
    patchKeyed(host, present.map((g) => [g, `<section><div class="group-title"><h2 class="h3">${g === "Review" ? "Worth a look" : g}</h2><span class="g-size"></span></div>
      <div class="items"></div>
      ${g === "Developer caches" ? `<p class="not-found" style="margin-top:8px">Package managers download these again the next time a project needs them, so they're off by default.</p>` : ""}</section>`]));
    for (const sec of host.children) {
      const its = items.filter((it) => it.group === sec.dataset.key);
      patch(sec.querySelector(".g-size"), fmtBytes(its.reduce((a, it) => a + it.size, 0)));
      patchKeyed(sec.querySelector(".items"), its.map((it) => [it.id, itemHTML(it)]));
    }
  }
  patch($("#clean-foot"), `${analyzing ? `<p class="not-found">${icon("loader-circle", "spin")} Still measuring. Sizes appear as each location is checked.</p>` : ""}
    ${missing.length && !analyzing ? `<p class="not-found">Not on this PC: ${esc(missing.map((m) => m.name).join(", "))}.</p>` : ""}`);
}

function cleanSummaryHTML() {
  const C = S.cleanup;
  const total = C.items.filter((it) => it.available && it.kind !== "review").reduce((a, it) => a + it.size, 0);
  const sel = selectedTotal();
  const running = C.status === "running", analyzing = C.status === "analyzing";
  const sysv = vol(S.sys.systemDrive);
  return `
      <div>
        <div class="readout"><span class="big">${fmtBytes(sel)}</span><span class="of">selected of ${fmtBytes(total)} found</span></div>
        ${sysv && sysv.ready ? afterBar(sysv, sel) : ""}
      </div>
      <button class="btn primary lg" data-action="cleanup-run" ${!sel || running || analyzing ? "disabled" : ""}>${running ? icon("loader-circle", "spin") + "Cleaning…" : icon("brush-cleaning") + `Clean up ${fmtBytes(sel)}`}</button>
    `;
}

function listNames(names) {
  if (names.length <= 2) return names.join(" and ");
  return `${names.slice(0, 2).join(", ")} and ${names.length - 2} more`;
}

function afterBar(v, sel) {
  const used = v.total - v.free;
  const rec = Math.min(sel, used);
  return `<div class="after">
    <div class="capbar"><span class="used" style="width:${((used - rec) / v.total) * 100}%"></span><span class="reclaim" style="width:${(rec / v.total) * 100}%"></span></div>
    <div class="cap-legend"><span>${esc(driveName(v))} after clean-up: <b>${fmtBytes(v.free + rec)}</b> free, up from ${fmtBytes(v.free)}</span></div>
  </div>`;
}

function resultBanner(r) {
  const bits = [`Freed ${fmtBytes(r.freed)}.`];
  if (r.skipped) bits.push(`${fmtInt(r.skipped)} files were in use and were left alone; they'll clear after a restart.`);
  return `<div class="note is-ok" style="margin:-8px 0 24px">${icon("circle-check")}<span class="note-title">${esc(bits[0])}</span><span class="note-text">${esc(bits.slice(1).join(" ") || "Everything selected was removed.")}</span></div>`;
}

function itemHTML(it) {
  const C = S.cleanup;
  const locked = it.needsAdmin && !S.sys.elevated;
  const st = C.states[it.id];
  const checked = C.selected.has(it.id) && selectable(it);
  const disabled = !selectable(it) || C.status === "running";
  if (it.kind === "review") {
    return `<div class="item is-locked">${icon("search")}
      <span class="item-name">${esc(it.name)}</span>
      <div class="item-side"><span class="item-size">${fmtBytes(it.size)}</span><button class="btn sm" data-action="review" data-path="${esc(it.reviewPath)}">Review in Space</button></div>
      <span class="item-desc">${esc(it.description)}</span></div>`;
  }
  const unit = it.kind === "recycle-bin" ? ["item", "items"] : ["file", "files"];
  let stateHTML = locked && !it.size ? `<span class="item-files">Can't measure without admin</span>` : `<span class="item-files">${fmtInt(it.files)} ${it.files === 1 ? unit[0] : unit[1]}</span>`;
  if (st?.running) stateHTML = `<span class="item-state">${icon("loader-circle", "spin")} ${st.freed ? `${fmtBytes(st.freed)} freed` : "Cleaning…"}</span>`;
  else if (st?.error && st.freed > 0) stateHTML = `<span class="item-state is-ok" title="${esc(st.error)}">${icon("check")} ${fmtBytes(st.freed)} freed, some folders left alone</span>`;
  else if (st?.error) stateHTML = `<span class="item-state is-bad">${esc(st.error)}</span>`;
  else if (st?.done) stateHTML = `<span class="item-state is-ok">${icon("check")} ${fmtBytes(st.freed)} freed${st.skipped ? `, ${fmtInt(st.skipped)} in use` : ""}</span>`;
  return `<label class="item ${locked ? "is-locked" : ""}">
    <input type="checkbox" class="check" data-action="toggle-item" data-id="${esc(it.id)}" ${checked ? "checked" : ""} ${disabled ? "disabled" : ""} aria-describedby="d-${esc(it.id)}" />
    <span class="item-name">${esc(it.name)}${locked ? `<span class="chip is-warn" title="Needs administrator rights">${icon("lock")}Admin</span>` : ""}</span>
    <div class="item-side"><span class="item-size">${it.size ? fmtBytes(it.size) : locked ? `<span class="muted">Unknown</span>` : `<span class="muted">Empty</span>`}</span>${stateHTML}</div>
    <span class="item-desc" id="d-${esc(it.id)}">${esc(it.description)}</span>
  </label>`;
}

// Measuring clean-up reads a lot of folders, so it never runs as part of launch:
//  - opening Clean up (or Measure again / F5) measures at full speed unless the result is under
//    30 minutes old;
//  - elsewhere, cached sizes (with "Measured X ago") are good enough for the badge and suggestions
//    for 12 hours; past that they're refreshed in the background once the app is idle, at low
//    priority (see scheduleIdleCleanup). So most launches measure nothing at all.
// A result is never trusted as fresh when it is `stale`: measured by the other elevation level
// (its own WebView2 profile, so its own localStorage), or older than the last clean-up.
const CLEANUP_FRESH_MS = 30 * 60 * 1000;
const CLEANUP_REFRESH_MS = 12 * 60 * 60 * 1000;
function cleanupFresh(maxAge = CLEANUP_FRESH_MS) {
  const C = S.cleanup, age = Date.now() - C.measuredAt;
  return C.measuredAt > 0 && !C.stale && age > -60000 && age < maxAge; // a clock set back counts as stale
}
// When a clean-up last started (set before cleanup_run, so closing mid-run still counts).
const CLEANED_KEY = "platter.cleanedAt";
// The last measure finished in this profile: { doneAt, startedAt }. The backend's cache entry is
// shared by the standard and elevated instances; a savedAt that doesn't match doneAt was written
// by the other one, which sees different items (admin-only folders, another account's profile).
const OWN_KEY = "platter.cleanupOwn";
function lsGet(k) { try { return localStorage.getItem(k); } catch { return null; } }
function lsSet(k, v) { try { localStorage.setItem(k, v); } catch {} }
function rememberMeasure(startedAt) {
  // A measure that started before the last clean-up holds sizes that are already gone: don't vouch for it.
  if (startedAt <= (Number(lsGet(CLEANED_KEY)) || 0)) return;
  lsSet(OWN_KEY, JSON.stringify({ doneAt: Date.now(), startedAt }));
}

// Each request gets a generation; a superseded run's result is dropped.
let cleanupGen = 0;
async function analyzeCleanup({ background = false } = {}) {
  const C = S.cleanup;
  if (C.status === "running") return;
  // The user asked while the slow background refresh runs: start a full-speed measure instead of
  // waiting on it (the background one finishes on its own; its result is ignored).
  if (C.status === "analyzing" && (background || !C.background)) return;
  const boost = C.status === "analyzing";
  const gen = ++cleanupGen;
  C.status = "analyzing"; C.background = background;
  rerenderCleanup();
  perf(background ? "js:cleanup-start-idle" : boost ? "js:cleanup-start-boost" : "js:cleanup-start");
  const startedAt = Date.now();
  let items = null, err = null;
  try { items = await invoke("cleanup_analyze", { background }); } catch (e) { err = e; }
  if (items) rememberMeasure(startedAt);
  if (gen !== cleanupGen) { perf("js:cleanup-dropped"); return; }
  if (items) {
    perf(background ? "js:cleanup-done-idle" : "js:cleanup-done-user");
    perf("js:cleanup-done");
    C.items = items;
    C.measuredAt = Date.now();
    // Still stale if a clean-up started while this was measuring.
    C.stale = startedAt <= (Number(lsGet(CLEANED_KEY)) || 0);
    if (!C.touched) C.selected = new Set(items.filter((it) => it.defaultOn && selectable(it)).map((it) => it.id));
  } else toast(errText(err), { tone: "bad" });
  C.status = "ready"; C.background = false;
  rerenderCleanup();
}

function measureIfStale() {
  if (!cleanupFresh()) analyzeCleanup({ background: false });
}

let idleCleanupT = 0;
function scheduleIdleCleanup(delay = 3000) {
  clearTimeout(idleCleanupT);
  idleCleanupT = setTimeout(() => idle(() => {
    if (!cleanupFresh(CLEANUP_REFRESH_MS)) analyzeCleanup({ background: true });
  }, 4000), delay);
}

function rerenderCleanup() {
  if (S.route.view === "cleanup") render();
  else {
    renderRail();
    if (S.route.view === "drive") {
      const v = vol(S.route.letter);
      if (v?.isSystem) { patch($("#capacity"), capacityHTML(v)); refreshOverviewBits(); }
    }
  }
}

async function runCleanup() {
  const C = S.cleanup;
  const ids = C.items.filter((it) => C.selected.has(it.id) && selectable(it)).map((it) => it.id);
  if (!ids.length) return;
  C.status = "running"; C.states = {}; C.lastResult = null;
  ids.forEach((id) => (C.states[id] = { running: true, freed: 0 }));
  // Mark the sizes out of date before deleting anything: if Platter is closed mid-run, the next
  // launch must not show what was freed as still reclaimable.
  C.stale = true;
  lsSet(CLEANED_KEY, String(Date.now()));
  render();
  try {
    const r = await invoke("cleanup_run", { ids });
    for (const x of r.items) C.states[x.id] = { done: !x.error, error: x.error, freed: x.freed, skipped: x.skipped };
    C.lastResult = r;
    toast(`Freed ${fmtBytes(r.freed)}.`);
  } catch (e) { toast(errText(e), { tone: "bad" }); ids.forEach((id) => delete C.states[id]); }
  C.status = "ready";
  await loadVolumes();
  render();
  // Re-measure so sizes reflect what's left.
  C.touched = true;
  analyzeCleanup({ background: false });
}

/* ------------------------------------------------------------------ Clean up sections */

const CLEAN_SUBS = [["caches", "Caches & leftovers"], ["apps", "Uninstall apps"], ["dupes", "Duplicates"]];
const CLEAN_SUB_TEXT = {
  caches: "Files that Windows and your apps recreate on their own. Your documents, photos, passwords and browsing history are never touched.",
  apps: "Remove apps with their own uninstaller, then clear out the folders, settings, services and tasks they leave behind.",
  dupes: "Find copies of the same file by their contents, not their names, plus near-identical photos and videos and copied folders.",
};
function cleanSub() {
  const s = S.route.sub;
  return CLEAN_SUBS.some(([k]) => k === s) ? s : "caches";
}
// Work a section needs when it's opened.
function enterCleanSub() {
  const sub = cleanSub();
  if (sub === "caches") measureIfStale();
  else if (sub === "apps") loadApps();
  else if (sub === "dupes") loadDupeRoots();
}
function setCleanSub(sub) {
  if (S.route.view !== "cleanup" || sub === cleanSub()) return;
  S.route.sub = sub;
  render({ resetScroll: true });
  saveRoute();
  enterCleanSub();
}
const isCleanSub = (sub) => S.route.view === "cleanup" && cleanSub() === sub;

// A section body is a fixed frame per mode; parts inside it are patched in place.
function frame(host, mode, html) {
  if (host.dataset.mode === mode) return false;
  host.dataset.mode = mode;
  host.innerHTML = html;
  return true;
}

/* ---------- Uninstall apps ---------- */

async function loadApps(force = false) {
  const A = S.apps;
  if (A.status === "loading" || (A.status === "ready" && !force)) return;
  A.status = "loading"; A.error = null;
  rerenderApps();
  try { A.list = await invoke("apps_list"); A.status = "ready"; }
  catch (e) { A.status = "error"; A.error = errText(e); }
  rerenderApps();
}
function rerenderApps() { if (isCleanSub("apps")) render(); }

function visibleApps() {
  const A = S.apps, q = A.q.trim().toLowerCase();
  const l = q ? A.list.filter((a) => `${a.name} ${a.publisher}`.toLowerCase().includes(q)) : A.list.slice();
  const byName = (a, b) => a.name.localeCompare(b.name, undefined, { sensitivity: "base" });
  if (A.sort === "size") l.sort((a, b) => b.size - a.size || byName(a, b));
  else if (A.sort === "date") l.sort((a, b) => (b.installDate || "").localeCompare(a.installDate || "") || byName(a, b));
  else l.sort(byName);
  return l;
}

function updateApps() {
  const A = S.apps;
  const host = $("#apps-body");
  if (!host) return;
  if (A.left) return updateLeftovers(host);
  patch($("#clean-head"), `<button class="btn" data-action="apps-reload" ${A.status === "loading" || A.busy ? "disabled" : ""}>${A.status === "loading" ? icon("loader-circle", "spin") + "Loading…" : icon("refresh-cw") + "Refresh list"}</button>`);
  if (frame(host, "list", `
      <div class="note orphan-note">${icon("scan-search")}<span class="note-title">Leftovers from apps you already removed</span>
        <span class="note-text">Uninstall entries for apps that are gone, services and scheduled tasks whose program is missing, startup entries that point nowhere, and program folders with nothing left to run.</span>
        <button class="btn" data-action="orphans">Look for leftovers</button></div>
      <div id="apps-notice"></div>
      <div class="apps-toolbar">
        <label class="search">${icon("search")}<span class="sr-only">Search apps</span><input class="input" id="apps-q" type="search" placeholder="Search apps" autocomplete="off" spellcheck="false" /></label>
        <div class="seg" id="apps-sort" role="group" aria-label="Sort apps"></div>
        <span class="apps-count num" id="apps-count"></span>
      </div>
      <div class="items apps-list" id="apps-list"></div>`)) $("#apps-q").value = A.q;
  // No other leftovers screen while an uninstaller runs: its result opens this app's leftovers.
  host.querySelector('[data-action="orphans"]').disabled = !!A.busy;

  const n = A.notice;
  const tone = { bad: ["is-danger", "circle-alert"], ok: ["is-ok", "circle-check"] }[n?.tone] || ["is-warn", "triangle-alert"];
  patch($("#apps-notice"), n ? `<div class="note ${tone[0]} apps-notice">${icon(tone[1])}<span class="note-title">${esc(n.title)}</span>
      <span class="note-text">${esc(n.text || "")}</span>${n.appId ? `<button class="btn sm" data-action="app-leftovers" data-id="${esc(n.appId)}">Look for leftovers</button>` : ""}</div>` : "");
  patch($("#apps-sort"), [["name", "Name"], ["size", "Size"], ["date", "Installed"]].map(([k, l]) => `<button data-action="apps-sort" data-v="${k}" aria-pressed="${A.sort === k}">${l}</button>`).join(""));

  const list = $("#apps-list");
  list.classList.toggle("is-empty", A.status === "error");
  if (A.status === "error") {
    patch($("#apps-count"), "");
    return patch(list, `<div class="empty">${icon("circle-alert")}<h2 class="h3">Couldn't list your apps</h2><p>${esc(A.error)}</p><div class="actions"><button class="btn" data-action="apps-reload">Try again</button></div></div>`);
  }
  if (A.status !== "ready" && !A.list.length) {
    patch($("#apps-count"), "");
    return patch(list, Array.from({ length: 6 }, () => `<div class="item app-row"><span class="app-mark"></span><span class="item-name"><span class="skeleton" style="width:180px"></span></span><span class="item-desc"><span class="skeleton" style="width:240px"></span></span></div>`).join(""));
  }
  const apps = visibleApps();
  const total = A.list.reduce((s, a) => s + (a.size || 0), 0);
  patch($("#apps-count"), A.q.trim() ? `${fmtInt(apps.length)} of ${fmtInt(A.list.length)} apps` : `${fmtInt(A.list.length)} apps · ${fmtBytes(total)}`);
  list._h = undefined;
  if (!apps.length) {
    list.innerHTML = `<div class="apps-none">${A.list.length ? `No apps match “${esc(A.q.trim())}”.` : "No desktop apps were found."}</div>`;
    return;
  }
  patchKeyed(list, apps.map((a) => [a.id, appRowHTML(a)]));
  hydrateAppIcons();
}

function appRowHTML(a) {
  const A = S.apps;
  const waiting = A.busy === a.id;
  const meta = [a.publisher, a.version && `Version ${a.version}`, a.installDate && `Installed ${shortDate(a.installDate)}`].filter(Boolean).join(" · ");
  const initial = (a.name.match(/[\p{L}\p{N}]/u)?.[0] || "?").toUpperCase();
  let side;
  if (waiting) side = `<span class="item-state">${icon("loader-circle", "spin")} Waiting for the uninstaller…</span><button class="link-btn" data-action="app-stop-wait">Stop waiting</button>`;
  else if (a.broken) side = `<button class="btn sm" data-action="app-leftovers" data-id="${esc(a.id)}" ${A.busy ? "disabled" : ""}>Remove leftovers</button>`;
  else side = `<button class="btn sm" data-action="app-uninstall" data-id="${esc(a.id)}" ${A.busy || !a.canUninstall ? "disabled" : ""}>Uninstall</button>`;
  return `<div class="item app-row${waiting ? " is-busy" : ""}">
    <span class="app-mark" aria-hidden="true" data-appicon="${esc(a.id)}">${esc(initial)}</span>
    <span class="item-name">${esc(a.name)}${a.broken ? `<span class="chip is-warn" title="Its uninstaller is gone. Platter can still remove what's left of it.">${icon("triangle-alert")}Uninstaller missing</span>` : ""}</span>
    <div class="item-side app-side"><span class="item-size">${a.size ? fmtBytes(a.size) : `<span class="muted" title="This app doesn't report its size">—</span>`}</span>${side}</div>
    <span class="item-desc">${esc(meta)}</span>
  </div>`;
}

async function uninstallApp(id) {
  const A = S.apps;
  const a = A.list.find((x) => x.id === id);
  if (!a || A.busy) return;
  const ok = await dialog({
    title: `Uninstall ${a.name}?`,
    body: `<p>${esc(a.name)}'s own uninstaller will open. Follow its steps.</p><p>When it's finished, Platter looks for anything it left behind (folders, settings, services and scheduled tasks) and lets you choose what to remove.</p>`,
    confirm: "Uninstall",
  });
  if (!ok || A.busy) return;
  A.busy = id; A.notice = null;
  rerenderApps();
  let r = null, err = null;
  try { r = await invoke("app_uninstall", { id }); } catch (e) { err = errText(e); }
  A.busy = null;
  if (err) {
    // A missing uninstaller can still be cleaned up after; a cancelled one can be tried again.
    A.notice = { tone: "bad", title: err, appId: /missing/i.test(err) ? id : null, name: a.name };
    rerenderApps();
    return;
  }
  await loadApps(true);
  if (r.removed) {
    A.list = A.list.filter((x) => x.id !== id); // its entry is gone, even if the reload didn't run
    toast(`${a.name} was uninstalled.`);
    // Never replace a leftovers screen the user is working in (a confirm may be open on it).
    if (A.left) A.notice = { tone: "ok", title: `${a.name} was uninstalled.`, text: "Look for anything it left behind when you're ready.", appId: id, name: a.name };
    else openLeftovers({ appId: id, name: a.name });
  } else {
    A.notice = {
      tone: "warn", title: `${a.name} still looks installed.`, appId: id, name: a.name,
      text: r.stoppedWaiting ? "Platter stopped waiting. If its uninstaller is still open, finish it, then look for leftovers." : "If you cancelled its uninstaller, nothing was removed.",
    };
    rerenderApps();
  }
}

// App icons load as rows scroll into view and are kept for the session. The letter tile stays
// for apps without one (command-line tools, drivers). The row HTML never contains the icon, so a
// row rebuilt by patchKeyed just gets it back from the cache.
const appIconCache = new Map(); // id -> data URL | "" (none) | Promise
let appIconObserver = null;
function hydrateAppIcons() {
  const els = $$(".app-mark[data-appicon]:not(.has-icon)");
  if (!els.length) return;
  appIconObserver ||= new IntersectionObserver((entries) => {
    for (const e of entries) if (e.isIntersecting) { appIconObserver.unobserve(e.target); loadAppIcon(e.target); }
  }, { root: $("#main"), rootMargin: "400px" });
  for (const el of els) {
    const url = appIconCache.get(el.dataset.appicon);
    if (typeof url === "string") setAppIcon(el, url); else appIconObserver.observe(el);
  }
}
function setAppIcon(el, url) {
  if (!url) return;
  el.classList.add("has-icon");
  el.innerHTML = `<img src="${esc(url)}" alt="" decoding="async" />`;
}
async function loadAppIcon(el) {
  const id = el.dataset.appicon;
  let p = appIconCache.get(id);
  if (p == null) { p = invoke("app_icon", { id }).then((u) => u || "", () => ""); appIconCache.set(id, p); }
  const url = await p;
  appIconCache.set(id, url);
  if (el.isConnected) setAppIcon(el, url);
}

/* ---------- Leftovers (one app's, or apps already removed) ---------- */

const LEFT_GROUPS = [
  ["files", "Folders and files", ["folder", "file"]],
  ["registry", "Registry entries", ["registry", "uninstall-entry"]],
  ["services", "Services", ["service"]],
  ["tasks", "Scheduled tasks", ["task"]],
  ["startup", "Startup entries", ["startup"]],
];
const LEFT_ICON = { folder: "folder", file: "file", registry: "key-round", "uninstall-entry": "package", service: "settings-2", task: "calendar-clock", startup: "power" };
const CONFIDENCE = {
  high: ["Belongs to it", "is-pine", "Platter is confident this belongs to the app."],
  medium: ["Probably", "", "This matches the app, but less certainly. Check the path before removing it."],
  low: ["Unsure", "is-warn", "A weak match. Only remove it if you recognise it."],
};
const leftLocked = (it) => it.needsAdmin && !S.sys.elevated;

async function openLeftovers({ appId = null, name = null } = {}) {
  const A = S.apps;
  // Still installed (its uninstaller was cancelled or is still open, or it was never run): what it
  // would leave behind includes its live install folder and the services running from it, so
  // nothing is picked for the user. An app whose uninstaller is missing counts as gone.
  const installed = !!appId && A.list.some((a) => a.id === appId && !a.broken);
  const L = { status: "loading", appId, name, installed, items: [], sel: new Set(), states: {}, result: null, error: null, scanId: 0 };
  A.left = L; A.notice = null;
  if (isCleanSub("apps")) render({ resetScroll: true });
  try {
    const scan = appId ? await invoke("app_leftovers", { id: appId }) : await invoke("orphan_leftovers");
    if (A.left !== L) return;
    L.scanId = scan.scanId; L.items = scan.items || []; L.status = "ready";
    if (scan.app) L.name = scan.app;
    L.sel = new Set(installed ? [] : L.items.filter((it) => it.defaultOn && !leftLocked(it)).map((it) => it.id));
  } catch (e) {
    if (A.left !== L) return;
    L.status = "error"; L.error = errText(e);
  }
  rerenderApps();
}

function leftSelected() {
  const L = S.apps.left;
  return L ? L.items.filter((it) => L.sel.has(it.id) && !leftLocked(it) && !L.states[it.id]?.ok) : [];
}

function updateLeftovers(host) {
  const L = S.apps.left;
  patch($("#clean-head"), "");
  const title = !L.appId ? "Leftovers from apps you already removed" : L.installed ? `What belongs to ${L.name || "the app"}` : `What ${L.name || "the app"} left behind`;
  frame(host, `left-${L.appId ?? "orphans"}-${L.scanId}`, `
    <div class="left-top"><button class="link-btn back" data-action="left-back">${icon("arrow-left")}All apps</button></div>
    <h2 class="section-title left-title" id="left-title"></h2>
    <p class="section-sub left-sub">Folders and files go to the Recycle Bin. Registry entries, services and scheduled tasks are saved to a backup folder before they're removed.</p>
    <section class="clean-summary" id="left-summary"></section>
    <div id="left-notices"></div>
    <div class="groups" id="left-groups"></div>`);
  patch($("#left-title"), esc(title));

  const groups = $("#left-groups");
  if (L.status === "loading") {
    patch($("#left-summary"), "");
    patch($("#left-notices"), "");
    return patch(groups, `<div class="progress-block" aria-live="polite"><div class="run-head"><h2 class="section-title">${icon("loader-circle", "spin")} Looking for leftovers…</h2></div><p class="muted" style="margin:0">Checking program folders, app data, the registry, services, scheduled tasks and startup entries.</p></div>`);
  }
  if (L.status === "error") {
    patch($("#left-summary"), "");
    patch($("#left-notices"), "");
    return patch(groups, `<div class="empty">${icon("circle-alert")}<h2 class="h3">Couldn't look for leftovers</h2><p>${esc(L.error)}</p><div class="actions"><button class="btn" data-action="left-retry">Try again</button><button class="btn ghost" data-action="left-back">Back to apps</button></div></div>`);
  }
  if (!L.items.length) {
    patch($("#left-summary"), "");
    patch($("#left-notices"), "");
    return patch(groups, `<div class="empty">${icon("circle-check")}<h2 class="h3">Nothing left behind</h2><p>${L.appId ? `Platter found no folders, settings, services or tasks left from ${esc(L.name || "this app")}.` : "Platter found nothing left over from apps that were removed."}</p><div class="actions"><button class="btn" data-action="left-back">Back to apps</button></div></div>`);
  }
  patch($("#left-summary"), leftSummaryHTML());
  const locked = L.items.filter((it) => leftLocked(it) && !L.states[it.id]?.ok);
  const r = L.result;
  patch($("#left-notices"), `${L.installed ? `<div class="note is-warn" style="margin:-8px 0 24px">${icon("triangle-alert")}<span class="note-title">${esc(L.name || "This app")} is still installed.</span>
      <span class="note-text">This list can include its install folder and the services and tasks it runs, so removing them can break it. Nothing is selected for you. To remove the app, uninstall it first.</span></div>` : ""}
    ${locked.length ? `<div class="admin-callout" style="margin:-8px 0 24px"><div>${icon("lock")}<span>${fmtInt(locked.length)} ${locked.length === 1 ? "item needs" : "items need"} administrator rights to remove (machine-wide folders, registry entries, services or tasks).</span></div><button class="btn sm" data-action="elevate">Restart as administrator</button></div>` : ""}
    ${r ? `<div class="note ${r.failed ? "is-warn" : "is-ok"}" style="margin:-8px 0 24px">${icon(r.failed ? "triangle-alert" : "circle-check")}<span class="note-title">Removed ${fmtInt(r.removed)} ${r.removed === 1 ? "item" : "items"}${r.freed ? ` and freed ${fmtBytes(r.freed)}` : ""}.</span>
      <span class="note-text">${r.failed ? `${fmtInt(r.failed)} couldn't be removed. The reason is shown next to each one.` : "Folders and files are in the Recycle Bin."}${r.backupFolder ? " A backup of the registry entries, services and tasks was saved." : ""}</span>
      ${r.backupFolder ? `<button class="btn sm" data-action="open-path" data-path="${esc(r.backupFolder)}">${icon("archive")}Open backup</button>` : ""}</div>` : ""}`);

  groups._h = undefined;
  const present = LEFT_GROUPS.filter(([, , kinds]) => L.items.some((it) => kinds.includes(it.kind)));
  patchKeyed(groups, present.map(([key, label]) => [key, `<section><div class="group-title"><h2 class="h3">${label}</h2><span class="g-size"></span></div><div class="items"></div></section>`]));
  for (const sec of groups.children) {
    const kinds = LEFT_GROUPS.find(([k]) => k === sec.dataset.key)[2];
    const its = L.items.filter((it) => kinds.includes(it.kind));
    const size = its.reduce((s, it) => s + (it.size || 0), 0);
    patch(sec.querySelector(".g-size"), size ? fmtBytes(size) : `${fmtInt(its.length)} ${its.length === 1 ? "entry" : "entries"}`);
    patchKeyed(sec.querySelector(".items"), its.map((it) => [it.id, leftItemHTML(it)]));
  }
}

function leftSummaryHTML() {
  const L = S.apps.left;
  const sel = leftSelected();
  const size = sel.reduce((s, it) => s + (it.size || 0), 0);
  const left = L.items.filter((it) => !L.states[it.id]?.ok).length;
  const removing = L.status === "removing";
  return `<div><div class="readout"><span class="big">${fmtInt(sel.length)}</span><span class="of">of ${fmtInt(left)} selected${size ? ` · ${fmtBytes(size)}` : ""}</span></div></div>
    <button class="btn primary lg" data-action="left-remove" ${!sel.length || removing ? "disabled" : ""}>${removing ? icon("loader-circle", "spin") + "Removing…" : icon("trash-2") + `Remove ${sel.length || ""} ${sel.length === 1 ? "item" : "items"}`}</button>`;
}

function leftItemHTML(it) {
  const L = S.apps.left;
  const locked = leftLocked(it);
  const st = L.states[it.id];
  const conf = [...(CONFIDENCE[it.confidence] || CONFIDENCE.low)];
  // Orphans belong to no app on the list, so "Belongs to it" would read oddly there.
  if (!L.appId && it.confidence === "high") conf[0] = "Left over";
  const disabled = locked || L.status === "removing" || st?.ok;
  let state = "";
  if (st?.ok) state = `<span class="item-state is-ok">${icon("check")} Removed</span>`;
  else if (st?.error) state = `<span class="item-state is-bad">${esc(st.error)}</span>`;
  else if (L.status === "removing" && L.sel.has(it.id) && !locked) state = `<span class="item-state">${icon("loader-circle", "spin")} Removing…</span>`;
  return `<label class="item left-item ${locked ? "is-locked" : ""} ${st?.ok ? "is-done" : ""}">
    <input type="checkbox" class="check" data-action="toggle-left" data-id="${esc(it.id)}" ${L.sel.has(it.id) && !st?.ok && !locked ? "checked" : ""} ${disabled ? "disabled" : ""} aria-describedby="ld-${esc(it.id)}" />
    <span class="item-name">${icon(LEFT_ICON[it.kind] || "file")}<span class="left-label">${esc(it.label)}</span><span class="chip ${conf[1]}" title="${esc(conf[2])}">${conf[0]}</span>${locked ? `<span class="chip is-warn" title="Needs administrator rights">${icon("lock")}Admin</span>` : ""}</span>
    <div class="item-side">${it.size ? `<span class="item-size">${fmtBytes(it.size)}</span>` : ""}${state}</div>
    <span class="item-desc" id="ld-${esc(it.id)}"><span class="left-path" title="${esc(it.path)}">${esc(it.path)}</span><span>${esc(it.reason)}</span></span>
  </label>`;
}

async function removeLeftovers() {
  const L = S.apps.left;
  if (!L || L.status !== "ready") return;
  const its = leftSelected();
  if (!its.length) return;
  const files = its.filter((it) => it.kind === "folder" || it.kind === "file").length;
  const other = its.length - files;
  const lines = [];
  if (L.installed) lines.push(`<p><b>${esc(L.name || "This app")} is still installed.</b> Removing parts of it can stop it working.</p>`);
  if (files) lines.push(`<p>${fmtInt(files)} ${files === 1 ? "folder or file goes" : "folders and files go"} to the Recycle Bin.</p>`);
  if (other) lines.push(`<p>${fmtInt(other)} ${other === 1 ? "registry entry, service or task is" : "registry entries, services or tasks are"} saved to a backup folder first, then removed.</p>`);
  const ok = await dialog({ title: `Remove ${fmtInt(its.length)} ${its.length === 1 ? "item" : "items"}?`, body: lines.join(""), confirm: "Remove", tone: L.installed ? "danger" : "primary" });
  if (!ok || S.apps.left !== L || L.status !== "ready") return;
  L.status = "removing"; L.result = null;
  for (const it of its) delete L.states[it.id];
  rerenderApps();
  try {
    const r = await invoke("leftovers_remove", { scanId: L.scanId, ids: its.map((it) => it.id) });
    for (const x of r.items || []) L.states[x.id] = { ok: x.ok, error: x.error };
    L.result = r;
    for (const it of its) if (L.states[it.id]?.ok) L.sel.delete(it.id);
    toast(r.failed ? `Removed ${fmtInt(r.removed)} of ${fmtInt(its.length)} items.` : `Removed ${fmtInt(r.removed)} ${r.removed === 1 ? "item" : "items"}.`, r.freed ? { action: "Open Recycle Bin", onAction: () => invoke("open_recycle_bin") } : {});
  } catch (e) { toast(errText(e), { tone: "bad" }); }
  L.status = "ready";
  rerenderApps();
  loadVolumes().then(() => renderRail());
  if (S.apps.status === "ready") loadApps(true); // an uninstall entry may be gone now
}

/* ---------- Duplicates ---------- */

const DUPES_KEY = "platter.dupes";
const DUPES_PHASE = { listing: "Finding files", hashing: "Comparing contents", images: "Comparing photos", videos: "Comparing videos", folders: "Comparing folders" };
const MIN_SIZES = [[100 * 1024, "100 KB"], [1024 * 1024, "1 MB"], [10 * 1024 * 1024, "10 MB"], [100 * 1024 * 1024, "100 MB"]];
const DEFAULT_OPTS = { minSize: 1024 * 1024, similarImages: true, similarVideos: true, folders: true };

function saveDupePrefs() {
  const D = S.dupes;
  lsSet(DUPES_KEY, JSON.stringify({ off: D.roots ? D.roots.filter((r) => !D.picked.has(r.path)).map((r) => r.path) : [], custom: D.custom, opts: D.opts }));
}

let rootsLoading = false;
async function loadDupeRoots() {
  const D = S.dupes;
  if (!D.opts) {
    let p = null;
    try { p = JSON.parse(lsGet(DUPES_KEY) || "null"); } catch {}
    D.opts = { ...DEFAULT_OPTS, ...(p?.opts || {}) };
    D.custom = Array.isArray(p?.custom) ? p.custom.filter((x) => typeof x === "string") : [];
    D.off = new Set(Array.isArray(p?.off) ? p.off : []);
  }
  if (D.roots || rootsLoading) return;
  rootsLoading = true;
  try { D.roots = await invoke("dupes_default_roots"); } catch { D.roots = []; }
  rootsLoading = false;
  D.picked = new Set(D.roots.filter((r) => !D.off.has(r.path)).map((r) => r.path));
  rerenderDupes();
}
function rerenderDupes() { if (isCleanSub("dupes")) render(); }

function dupeRoots() {
  const D = S.dupes;
  return [...(D.roots || []).filter((r) => D.picked?.has(r.path)).map((r) => r.path), ...D.custom];
}

async function startDupes() {
  const D = S.dupes;
  if (D.status === "running") return;
  const roots = dupeRoots();
  if (!roots.length) return toast("Choose at least one folder to look in.", { tone: "bad" });
  // D.scanId is the running scan's id (for its events). Deleting uses D.result.scanId, the scan
  // the results on screen came from, which stays the latest one if this scan stops or fails.
  D.status = "running"; D.scanId = null; D.early = []; D.error = null; D.lastDelete = null; D.cancelPending = false;
  D.progress = { phase: "listing", files: 0, bytes: 0, done: 0, total: 0, current: "" };
  render({ resetScroll: true });
  try { D.scanId = await invoke("dupes_start", { roots, options: D.opts }); }
  catch (e) { D.status = D.result ? "done" : "setup"; D.early = null; D.cancelPending = false; toast(errText(e), { tone: "bad" }); rerenderDupes(); return; }
  // Stop was clicked while dupes_start was still checking the folders (before a scan existed to cancel).
  if (D.cancelPending) { D.cancelPending = false; invoke("dupes_cancel").catch(() => {}); }
  // Events can arrive before dupes_start returns its id: replay the ones for this scan.
  const early = D.early || []; D.early = null;
  for (const [kind, p] of early) dupesEvent(kind, p);
}

function dupesEvent(kind, p) {
  const D = S.dupes;
  if (D.status !== "running") return;
  if (D.scanId == null) { D.early?.push([kind, p]); return; }
  if (p.scanId !== D.scanId) return;
  if (kind === "progress") onDupesProgress(p); else onDupesDone(p);
}

function onDupesProgress(p) {
  const D = S.dupes;
  D.progress = p;
  const set = (id, val) => { const el = document.getElementById(id); if (el) el.textContent = val; };
  set("dp-phase", DUPES_PHASE[p.phase] || "Working…");
  set("dp-files", fmtInt(p.files)); set("dp-bytes", fmtBytes(p.bytes)); set("dp-current", p.current || "");
  const bar = $("#dp-bar"), wrap = $("#dp-bar-wrap");
  if (bar && wrap) {
    const known = p.phase !== "listing" && p.total > 0;
    wrap.classList.toggle("is-indeterminate", !known);
    if (known) bar.style.width = `${Math.min(100, pct(p.done, p.total))}%`;
  }
}

async function onDupesDone(d) {
  const D = S.dupes;
  if (!d.ok) {
    D.status = d.cancelled ? (D.result ? "done" : "setup") : "error";
    D.error = d.cancelled ? null : d.error || "The scan stopped unexpectedly.";
    rerenderDupes();
    return;
  }
  const id = d.scanId;
  try {
    const r = await invoke("dupes_result", { scanId: id });
    if (D.scanId !== id) return;
    D.result = r; D.finishedAt = Date.now(); D.shown = 120;
    indexDupes();
    D.sel = recommendedDupes();
    D.section = ["files", "folders", "images", "videos"].find((s) => dupeSection(s).length) || "files";
    D.status = "done";
  } catch (e) { if (D.scanId !== id) return; D.status = "error"; D.error = errText(e); }
  rerenderDupes();
}

function dupeSection(s) {
  const r = S.dupes.result;
  if (!r) return [];
  return s === "files" ? r.groups : s === "folders" ? r.folders : s === "images" ? r.images : r.videos;
}
const groupMembers = (g) => g.folders || g.files; // folder groups also have `files` (a count)

// path -> { size, gid, folder: bool, protected: bool } for every copy shown.
function indexDupes() {
  const D = S.dupes, idx = new Map();
  for (const s of ["files", "folders", "images", "videos"]) {
    for (const g of dupeSection(s)) for (const f of groupMembers(g)) idx.set(f.path, { size: f.size, gid: g.id, folder: s === "folders", protected: f.risk === "protected", section: s });
  }
  D.index = idx;
  D.groups = new Map();
  for (const s of ["files", "folders", "images", "videos"]) for (const g of dupeSection(s)) D.groups.set(g.id, g);
}
// Folder groups and file groups pick their keepers separately, so the recommended copies can
// still add up to every copy of a file (a selected folder holding the file group's keeper).
function recommendedDupes() {
  const sel = new Set();
  for (const s of ["files", "folders"]) for (const g of dupeSection(s)) for (const f of groupMembers(g)) if (f.select && f.risk !== "protected") sel.add(f.path);
  return keepOneOfEach(sel).sel;
}
const lower = (p) => p.toLowerCase();
const inside = (p, dir) => { const a = lower(p), b = lower(dir).replace(/\\+$/, ""); return a.startsWith(b + "\\"); };

const sendable = (p) => { const x = S.dupes.index.get(p); return !!x && !x.protected; };
const selectedFolders = (sel) => [...sel].filter((p) => sendable(p) && S.dupes.index.get(p).folder);

// Selected copies, without the ones that sit inside a selected folder (counted once, sent once).
function selectedDupes(sel = S.dupes.sel) {
  const D = S.dupes;
  const picks = [...sel].map((p) => ({ path: p, ...(D.index.get(p) || {}) })).filter((x) => x.gid && !x.protected);
  const dirs = picks.filter((x) => x.folder).map((x) => x.path);
  return picks.filter((x) => !dirs.some((d) => d !== x.path && inside(x.path, d)));
}

// Groups (of any kind) whose every copy this selection removes: selected, or inside a selected
// folder copy. dupes_delete refuses the whole request if there is one.
function groupsLosingEveryCopy(sel) {
  const dirs = selectedFolders(sel);
  const gone = (p) => (sel.has(p) && sendable(p)) || dirs.some((d) => inside(p, d));
  return [...S.dupes.groups.values()].filter((g) => groupMembers(g).every((f) => gone(f.path)));
}

// The selection minus what it takes to leave a copy in every group. Per group, it frees the
// first copy (keeper order) that is selected on its own, or else the keeper together with the
// selected folder copies holding it. One pass is enough: unselecting never takes a copy away.
function keepOneOfEach(sel) {
  const out = new Set(sel), dropped = [];
  let dirs = selectedFolders(out);
  const gone = (p) => (out.has(p) && sendable(p)) || dirs.some((d) => inside(p, d));
  for (const g of S.dupes.groups.values()) {
    const ms = groupMembers(g);
    if (!ms.every((f) => gone(f.path))) continue;
    const free = ms.find((f) => out.has(f.path) && !dirs.some((d) => inside(f.path, d))) || ms[0];
    for (const p of [free.path, ...dirs.filter((d) => inside(free.path, d))]) if (out.delete(p)) dropped.push(p);
    dirs = dirs.filter((d) => out.has(d));
  }
  return { sel: out, dropped };
}

function updateDupes() {
  const D = S.dupes;
  const host = $("#dupes-body");
  if (!host) return;
  if (!D.opts) loadDupeRoots();
  if (D.status === "running") return updateDupesRunning(host);
  if (D.status === "done" && D.result) return updateDupesDone(host);
  updateDupesSetup(host);
}

function updateDupesSetup(host) {
  const D = S.dupes;
  patch($("#clean-head"), D.result ? `<button class="btn" data-action="dupes-show">${icon("arrow-left")}Back to results</button>` : "");
  frame(host, "setup", `<div id="dupes-error"></div>
    <div class="dsetup">
      <section class="dsetup-block"><h2 class="section-title">Where to look</h2><p class="section-sub">Your own folders are a good start. Add a drive to look everywhere; Windows and app folders on it are skipped.</p><div class="root-chips" id="dupes-roots"></div></section>
      <section class="dsetup-block"><h2 class="section-title">What to find</h2><div class="opt-list" id="dupes-opts"></div></section>
      <div class="dsetup-go" id="dupes-go"></div>
    </div>`);
  patch($("#dupes-error"), D.status === "error" ? `<div class="note is-danger" style="margin-bottom:20px">${icon("circle-alert")}<span class="note-title">The last scan didn't finish.</span><span class="note-text">${esc(D.error)}</span></div>` : "");
  const roots = D.roots;
  patch($("#dupes-roots"), roots == null ? `<span class="skeleton" style="width:320px;height:30px"></span>` : `
    ${roots.map((r) => `<label class="root-chip"><input type="checkbox" class="check" data-root="${esc(r.path)}" ${D.picked.has(r.path) ? "checked" : ""} /><span title="${esc(r.path)}">${esc(r.label)}</span></label>`).join("")}
    ${D.custom.map((p) => `<span class="root-chip is-custom">${icon("folder")}<span title="${esc(p)}">${esc(rootLabel(p))}</span><button class="root-x" data-action="dupes-remove-root" data-path="${esc(p)}" aria-label="Stop looking in ${esc(p)}" title="Remove">${icon("x")}</button></span>`).join("")}
    <button class="btn sm" data-action="dupes-add-root">${icon("plus")}Add a folder or drive</button>`);
  const o = D.opts || DEFAULT_OPTS;
  const opt = (key, title, text) => `<label class="opt"><input type="checkbox" class="check" data-opt="${key}" ${o[key] ? "checked" : ""} /><span><b>${title}</b><span>${text}</span></span></label>`;
  patch($("#dupes-opts"), `
    <label class="opt is-fixed"><input type="checkbox" class="check" checked disabled /><span><b>Identical files</b><span>The same content byte for byte, whatever the file is called.</span></span></label>
    ${opt("similarImages", "Similar photos", "Resized or re-saved copies and burst shots that look almost the same.")}
    ${opt("similarVideos", "Similar videos", "The same clip saved at another size or quality.")}
    ${opt("folders", "Duplicate folders", "Whole folders with the same files inside, like an old copy of a backup.")}
    <div class="opt-size"><label for="dupes-min">Skip files smaller than</label><select class="select" id="dupes-min">${MIN_SIZES.map(([v, l]) => `<option value="${v}" ${o.minSize === v ? "selected" : ""}>${l}</option>`).join("")}</select></div>`);
  const n = dupeRoots().length;
  patch($("#dupes-go"), `<button class="btn primary lg" data-action="dupes-start" ${!n ? "disabled" : ""}>${icon("scan-search")}Find duplicates</button><span class="muted">${n ? "Nothing is removed until you choose." : "Choose at least one folder."}</span>`);
}
function rootLabel(p) {
  if (/^[A-Za-z]:\\?$/.test(p)) { const v = vol(p[0].toUpperCase()); return v ? driveName(v) : `Drive ${p[0].toUpperCase()}:`; }
  return p.replace(/\\+$/, "").split("\\").pop() || p;
}

function updateDupesRunning(host) {
  const D = S.dupes, p = D.progress || {};
  patch($("#clean-head"), "");
  if (frame(host, "running", `<div class="progress-block" aria-live="polite">
      <div class="run-head"><h2 class="section-title" id="dp-phase"></h2><button class="btn" data-action="dupes-cancel">${icon("x")}Stop</button></div>
      <div class="progress-figs">
        <div class="fig"><b id="dp-files"></b><span>files looked at</span></div>
        <div class="fig"><b id="dp-bytes"></b><span>in those files</span></div>
      </div>
      <div class="bar is-indeterminate" id="dp-bar-wrap"><span id="dp-bar"></span></div>
      <div class="current-path" id="dp-current"></div>
      <p class="muted dp-hint">Platter reads only files that share a size with another file, so this goes faster than it looks.</p>
    </div>`)) onDupesProgress(p);
}

function updateDupesDone(host) {
  const D = S.dupes, r = D.result;
  patch($("#clean-head"), `<span class="clean-age">Found ${esc(relTime(D.finishedAt))}</span><button class="btn" data-action="dupes-setup" ${D.deleting ? "disabled" : ""}>Change folders</button><button class="btn" data-action="dupes-start" ${D.deleting ? "disabled" : ""}>${icon("refresh-cw")}Scan again</button>`);
  const nothing = !r.groups.length && !r.folders.length && !r.images.length && !r.videos.length;
  frame(host, nothing ? "none" : "done", nothing ? `<div id="dupes-none"></div>` : `
    <section class="clean-summary" id="dupes-summary"></section>
    <div id="dupes-notices"></div>
    <div class="dupes-bar"><div class="seg" id="dupes-sections" role="group" aria-label="Kinds of duplicates"></div><div class="dupes-sel" id="dupes-sel"></div></div>
    <div class="dgroups" id="dupes-groups"></div>
    <div id="dupes-more"></div>`);
  const scanned = `Looked at ${fmtInt(r.files)} files (${fmtBytes(r.bytes)}) in ${listNames(r.roots.map(rootLabel))}.`;
  if (nothing) {
    return patch($("#dupes-none"), `<div class="empty">${icon("circle-check")}<h2 class="h3">No duplicates found</h2><p>${esc(scanned)} No file appears twice${D.opts?.similarImages ? ", and no photos look alike" : ""}.</p><div class="actions"><button class="btn" data-action="dupes-setup">Look somewhere else</button></div></div>`);
  }
  patch($("#dupes-summary"), dupesSummaryHTML());
  const ld = D.lastDelete;
  const skippedBits = [];
  if (r.skipped.denied) skippedBits.push(`${fmtInt(r.skipped.denied)} ${r.skipped.denied === 1 ? "folder" : "folders"} Windows wouldn't open`);
  if (r.skipped.cloud) skippedBits.push(`${fmtInt(r.skipped.cloud)} online-only ${r.skipped.cloud === 1 ? "file" : "files"}`);
  patch($("#dupes-notices"), `${ld?.failed?.length ? `<div class="note is-warn" style="margin:-8px 0 20px">${icon("triangle-alert")}<span class="note-title">${fmtInt(ld.failed.length)} ${ld.failed.length === 1 ? "item was" : "items were"} left in place.</span><span class="note-text">${esc(ld.failed.slice(0, 3).map((f) => `${rootLabel(f.path)}: ${f.error}`).join(" "))}${ld.failed.length > 3 ? ` And ${fmtInt(ld.failed.length - 3)} more.` : ""}</span></div>` : ""}
    <p class="dupes-scanned">${esc(scanned)}${skippedBits.length ? ` Skipped ${esc(listNames(skippedBits))}.` : ""}</p>`);
  const sections = [["files", "Identical files"], ["folders", "Folders"], ["images", "Similar photos"], ["videos", "Similar videos"]].filter(([s]) => dupeSection(s).length);
  if (!sections.some(([s]) => s === D.section)) D.section = sections[0][0];
  patch($("#dupes-sections"), sections.map(([s, l]) => `<button data-action="dupes-section" data-v="${s}" aria-pressed="${D.section === s}">${l}<span class="count">${fmtInt(dupeSection(s).length)}</span></button>`).join(""));
  patch($("#dupes-sel"), `<button class="link-btn" data-action="dupes-recommended" ${D.deleting ? "disabled" : ""}>Select recommended</button><button class="link-btn" data-action="dupes-clear" ${D.deleting || !D.sel.size ? "disabled" : ""}>Clear selection</button>`);
  const list = dupeSection(D.section);
  const shown = list.slice(0, D.shown);
  const host2 = $("#dupes-groups");
  host2.dataset.section !== D.section && (host2.innerHTML = "", host2.dataset.section = D.section);
  patchKeyed(host2, shown.map((g) => [g.id, D.section === "images" || D.section === "videos" ? simGroupHTML(g) : dupGroupHTML(g, D.section === "folders")]));
  patch($("#dupes-more"), list.length > shown.length ? `<div class="dupes-more"><button class="btn" data-action="dupes-more">Show ${fmtInt(Math.min(120, list.length - shown.length))} more</button><span class="muted">${fmtInt(list.length - shown.length)} more ${list.length - shown.length === 1 ? "group" : "groups"}</span></div>` : "");
  hydrateThumbs();
}

function dupesSummaryHTML() {
  const D = S.dupes, r = D.result;
  const picks = selectedDupes();
  const size = picks.reduce((s, x) => s + (x.size || 0), 0);
  return `<div><div class="readout"><span class="big">${fmtBytes(size)}</span><span class="of">selected · ${fmtBytes(r.reclaimable)} recommended</span></div>
      <p class="dupes-hint">Recommended copies are the extra ones in Downloads, Temp, or named like a copy. Similar photos and videos are never picked for you.</p></div>
    <button class="btn primary lg" data-action="dupes-delete" ${!picks.length || D.deleting ? "disabled" : ""}>${D.deleting ? icon("loader-circle", "spin") + "Moving…" : icon("trash-2") + (picks.length ? `Move ${fmtInt(picks.length)} ${picks.some((x) => x.folder) ? (picks.length === 1 ? "item" : "items") : picks.length === 1 ? "copy" : "copies"} to Recycle Bin` : "Move to Recycle Bin")}</button>`;
}

function riskChip(f) {
  if (f.keep) return `<span class="chip is-pine">${icon("check")}Keep</span>`;
  if (f.risk === "protected") return `<span class="chip is-warn" title="Part of an app, game or project folder. Platter leaves it alone.">${icon("lock")}Leave</span>`;
  return "";
}

function dupGroupHTML(g, isFolder) {
  const D = S.dupes;
  const members = groupMembers(g);
  const names = new Set(members.map((f) => f.name.toLowerCase()));
  const title = isFolder ? members[0].name : members[0].name;
  const meta = isFolder
    ? `${members.length} copies · ${fmtInt(g.files)} ${g.files === 1 ? "file" : "files"}, ${fmtBytes(g.size)} each`
    : `${members.length} copies · ${fmtBytes(g.size)} each`;
  return `<section class="dgroup">
    <header class="dgroup-head">${isFolder ? icon("folder") : `<span class="cat-dot" style="background:${catVar(g.cat)}" title="${esc(CATS[g.cat] || "Other")}"></span>`}
      <span class="dgroup-name">${esc(title)}${names.size > 1 ? `<span class="muted"> and ${names.size - 1} other ${names.size === 2 ? "name" : "names"}</span>` : ""}</span>
      <span class="dgroup-meta num">${meta}</span></header>
    <div class="dfiles">${members.map((f) => dupFileHTML(f, g, names.size > 1)).join("")}</div>
  </section>`;
}

function dupFileHTML(f, g, showName) {
  const D = S.dupes;
  const prot = f.risk === "protected";
  const checked = D.sel.has(f.path) && !prot;
  return `<label class="dfile${checked ? " is-sel" : ""}">
    <input type="checkbox" class="check" data-action="toggle-dupe" data-path="${esc(f.path)}" data-gid="${esc(g.id)}" ${checked ? "checked" : ""} ${prot || D.deleting ? "disabled" : ""} aria-label="Move ${esc(f.path)} to the Recycle Bin" />
    <span class="dfile-main"><span class="dfile-path" title="${esc(f.path)}">${showName ? `<b>${esc(f.name)}</b> in ` : ""}${esc(f.folder)}</span>
      <span class="dfile-note">${riskChip(f)}<span>${esc(f.note)}</span></span></span>
    <span class="dfile-date num" title="Last changed">${f.modified ? esc(shortDate(f.modified)) : ""}</span>
    <button class="btn sm icon ghost" data-action="reveal" data-path="${esc(f.path)}" title="Show in File Explorer" aria-label="Show ${esc(f.name)} in File Explorer">${icon("folder-open")}</button>
  </label>`;
}

function simGroupHTML(g) {
  const sims = g.files.filter((f) => !f.keep).map((f) => f.similarity);
  const lo = Math.min(...sims), hi = Math.max(...sims);
  const what = g.kind === "video" ? "videos" : "photos";
  return `<section class="dgroup">
    <header class="dgroup-head">${icon(g.kind === "video" ? "film" : "image")}<span class="dgroup-name">${g.files.length} similar ${what}</span>
      <span class="dgroup-meta num">${sims.length ? (lo === hi ? `${lo}% alike` : `${lo}–${hi}% alike`) : ""}</span></header>
    <div class="sim-grid">${g.files.map((f) => simTileHTML(f, g)).join("")}</div>
  </section>`;
}

function fmtDuration(ms) {
  const s = Math.round((ms || 0) / 1000), h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60), sec = s % 60;
  return h ? `${h}:${String(m).padStart(2, "0")}:${String(sec).padStart(2, "0")}` : `${m}:${String(sec).padStart(2, "0")}`;
}

function simTileHTML(f, g) {
  const D = S.dupes;
  const prot = f.risk === "protected";
  const checked = D.sel.has(f.path) && !prot;
  const dims = f.width && f.height ? `${f.width} × ${f.height}` : "";
  const meta = [g.kind === "video" && f.durationMs != null ? fmtDuration(f.durationMs) : dims, fmtBytes(f.size)].filter(Boolean).join(" · ");
  return `<label class="sim-tile${checked ? " is-sel" : ""}">
    <span class="thumb" data-thumb="${esc(f.path)}">${icon(g.kind === "video" ? "film" : "image")}</span>
    <span class="sim-row"><input type="checkbox" class="check" data-action="toggle-dupe" data-path="${esc(f.path)}" data-gid="${esc(g.id)}" ${checked ? "checked" : ""} ${prot || D.deleting ? "disabled" : ""} aria-label="Move ${esc(f.path)} to the Recycle Bin" />
      <span class="sim-name" title="${esc(f.path)}">${esc(f.name)}</span></span>
    <span class="sim-meta num">${esc(meta)}</span>
    <span class="sim-meta">${f.keep ? `<span class="chip is-pine">${icon("check")}Best copy</span>` : prot ? riskChip(f) : `${f.similarity}% alike`}</span>
    <span class="sim-meta sim-folder" title="${esc(f.folder)}">${esc(f.folder)}</span>
  </label>`;
}

// Thumbnails load as tiles scroll into view and are kept for the session.
const thumbCache = new Map();
let thumbObserver = null;
function hydrateThumbs() {
  const els = $$(".thumb[data-thumb]:not(.has-img)");
  if (!els.length) return;
  thumbObserver ||= new IntersectionObserver((entries) => {
    for (const e of entries) if (e.isIntersecting) { thumbObserver.unobserve(e.target); loadThumb(e.target); }
  }, { root: $("#main"), rootMargin: "300px" });
  for (const el of els) {
    const url = thumbCache.get(el.dataset.thumb);
    if (typeof url === "string") setThumb(el, url); else thumbObserver.observe(el);
  }
}
function setThumb(el, url) {
  if (!url) return;
  el.classList.add("has-img");
  el.innerHTML = `<img src="${esc(url)}" alt="" loading="lazy" decoding="async" />`;
}
async function loadThumb(el) {
  const path = el.dataset.thumb;
  let p = thumbCache.get(path);
  if (!p) { p = invoke("dupes_thumb", { path }).catch(() => ""); thumbCache.set(path, p); }
  const url = await p;
  thumbCache.set(path, url);
  if (el.isConnected) setThumb(el, url);
}

function toggleDupe(el) {
  const D = S.dupes;
  const path = el.dataset.path, g = D.groups?.get(el.dataset.gid);
  if (!g) return;
  if (el.checked) {
    const next = new Set(D.sel); next.add(path);
    // Selecting more only ever removes more, so a group newly losing every copy raises the count.
    const lost = groupsLosingEveryCopy(next);
    if (lost.length > groupsLosingEveryCopy(D.sel).length) {
      el.checked = false; // the row's HTML is unchanged, so set the box back directly
      const other = lost.find((x) => !groupMembers(x).some((f) => f.path === path));
      toast(other ? `That would remove every copy of “${groupMembers(other)[0].name}”. Platter keeps at least one.` : "Platter keeps at least one copy. Unselect another copy first.", { tone: "bad" });
      return;
    }
    D.sel.add(path);
  } else D.sel.delete(path);
  restoringFocus(() => {
    patch($("#dupes-summary"), dupesSummaryHTML());
    patch($("#dupes-sel"), `<button class="link-btn" data-action="dupes-recommended">Select recommended</button><button class="link-btn" data-action="dupes-clear" ${!D.sel.size ? "disabled" : ""}>Clear selection</button>`);
    const host = $("#dupes-groups");
    const sec = [...(host?.children || [])].find((c) => c.dataset.key === g.id);
    if (sec) {
      const html = D.section === "images" || D.section === "videos" ? simGroupHTML(g) : dupGroupHTML(g, D.section === "folders");
      const n = fromHTML(html); n.dataset.key = g.id; n._h = html; sec.replaceWith(n);
      hydrateThumbs();
    }
  });
}

async function deleteDupes() {
  const D = S.dupes;
  if (D.deleting || !D.result) return;
  // Leave out whatever would remove every copy of something, rather than have the whole request refused.
  const { sel, dropped } = keepOneOfEach(D.sel);
  const picks = selectedDupes(sel);
  if (!picks.length) {
    if (dropped.length) toast("That's every copy there is. Platter keeps at least one, so unselect one copy.", { tone: "bad" });
    return;
  }
  const size = picks.reduce((s, x) => s + (x.size || 0), 0);
  const folders = picks.filter((x) => x.folder).length;
  const what = folders === picks.length ? (folders === 1 ? "folder" : "folders") : folders ? "items" : picks.length === 1 ? "file" : "files";
  const kept = dropped.length ? `<p>${fmtInt(dropped.length)} selected ${dropped.length === 1 ? "item stays" : "items stay"} where ${dropped.length === 1 ? "it is" : "they are"}, because with the rest ${dropped.length === 1 ? "it" : "they"} would remove every copy of a file.</p>` : "";
  const ok = await dialog({
    title: `Move ${fmtInt(picks.length)} ${what} to the Recycle Bin?`,
    body: `<p>That's ${fmtBytes(size)}. You can restore them from the Recycle Bin; the space is freed when it's emptied.</p>${kept}<p>Platter keeps at least one copy of everything and skips any file that changed since the scan.</p>`,
    confirm: "Move to Recycle Bin",
  });
  if (!ok || D.deleting || D.status !== "done" || !D.result) return;
  // Delete against the results on screen: a later scan that was stopped or failed leaves them the latest.
  const scanId = D.result.scanId;
  D.sel = sel;
  D.deleting = true; D.lastDelete = null;
  rerenderDupes();
  try {
    const r = await invoke("dupes_delete", { scanId, paths: picks.map((x) => x.path) });
    D.lastDelete = r;
    if (r.removed) toast(`Moved ${fmtInt(r.removed)} ${r.removed === 1 ? "item" : "items"} (${fmtBytes(r.freed)}) to the Recycle Bin.`, { action: "Open Recycle Bin", onAction: () => invoke("open_recycle_bin") });
    else if (r.failed?.length) toast("Nothing was moved. See the reasons below.", { tone: "bad" });
    try { D.result = await invoke("dupes_result", { scanId }); indexDupes(); } catch {}
    const failed = new Set((r.failed || []).map((f) => f.path));
    D.sel = new Set([...D.sel].filter((p) => D.index.has(p) && failed.has(p)));
  } catch (e) { toast(errText(e), { tone: "bad" }); }
  D.deleting = false;
  rerenderDupes();
  loadVolumes().then(() => renderRail());
}

/* ------------------------------------------------------------------ events from Rust */

function onScanProgress(p) {
  if (!scanEventIsMine("progress", p)) return;
  S.scan.progress = p;
  const set = (id, val) => { const el = document.getElementById(id); if (el) el.textContent = val; };
  set("sp-bytes", fmtBytes(p.bytes)); set("sp-files", fmtInt(p.files)); set("sp-dirs", fmtInt(p.dirs)); set("sp-current", p.current);
  const v = vol(S.scan.letter); const bar = $("#sp-bar");
  if (bar && S.scan.isRoot && v) bar.style.width = `${Math.min(100, pct(p.bytes, v.total - v.free))}%`;
}
async function onScanDone(d) {
  if (!scanEventIsMine("done", d)) return;
  const sc = S.scan;
  if (!d.ok) {
    S.scan = d.cancelled ? { status: "idle" } : { status: "error", letter: sc.letter, path: sc.path, error: d.error || "The scan stopped unexpectedly." };
    if (S.route.view === "drive" && S.route.tab === "space") renderPanel();
    return;
  }
  sc.done = d;
  sc.status = "done";
  sc.trail = [];
  sc.topFiles = null;
  await openNode(d.rootId);
}
listen("scan-progress", onScanProgress);
listen("scan-done", onScanDone);

listen("optimize-progress", (p) => {
  const r = S.opt.running; if (!r) return;
  Object.assign(r, { percent: p.percent ?? r.percent, stage: p.stage || r.stage, line: p.line });
  const set = (id, val) => { const el = document.getElementById(id); if (el) el.textContent = val; };
  set("op-stage", r.stage); set("op-pct", r.percent != null ? `${Math.round(r.percent)}%` : ""); set("op-line", r.line);
  const bar = $("#op-bar"); if (bar && r.percent != null) { bar.style.width = `${r.percent}%`; $("#op-bar-wrap")?.classList.remove("is-indeterminate"); }
});
listen("optimize-done", async (d) => {
  S.opt.running = null;
  S.opt.results[d.letter] = d;
  toast(d.message, { tone: d.ok ? "ok" : "bad" });
  await loadHistory();
  // Manage shows nothing from the optimizer; leave it alone so a name being typed there survives.
  if (S.route.view === "drive") { if (S.route.tab !== "manage") renderPanel(); loadMap(d.letter, true); }
});

listen("cleanup-item", (it) => {
  const C = S.cleanup; if (C.status !== "analyzing") return;
  const i = C.items.findIndex((x) => x.id === it.id);
  if (i >= 0) C.items[i] = it; else C.items.push(it);
  if (!C.touched && it.defaultOn && selectable(it)) C.selected.add(it.id);
  clearTimeout(C.t); C.t = setTimeout(rerenderCleanup, 60);
});
listen("cleanup-progress", (p) => {
  const st = S.cleanup.states[p.id]; if (!st) return;
  st.freed = p.freed;
  if (p.done) { st.running = false; st.done = true; }
  if (S.route.view === "cleanup") { clearTimeout(S.cleanup.t); S.cleanup.t = setTimeout(render, 50); }
});

listen("dupes-progress", (p) => dupesEvent("progress", p));
listen("dupes-done", (d) => dupesEvent("done", d));

listen("drives-changed", async () => {
  const ident = (v) => `${v.serial}|${v.total}|${v.fileSystem}|${v.ready}|${v.status ?? ""}`;
  const beforeVols = new Map(S.volumes.map((v) => [v.letter, ident(v)]));
  const before = new Set(beforeVols.keys());
  await loadVolumes();
  const after = new Set(S.volumes.map((v) => v.letter));
  for (const v of S.volumes) if (!before.has(v.letter)) toast(`${driveName(v)} connected.`, { action: "Open", onAction: () => go({ view: "drive", letter: v.letter }) });
  for (const l of before) if (!after.has(l)) toast(`Drive ${l}: was removed.`);
  // Caches are keyed by letter: a different drive can now sit behind the same letter.
  const lettersSeen = new Set([...before, ...after, ...Object.keys(S.maps), ...Object.keys(S.checks), ...Object.keys(S.opt.results)]);
  for (const l of lettersSeen) {
    const v = vol(l);
    if (!v || !beforeVols.has(l) || beforeVols.get(l) !== ident(v)) forgetLetter(l);
  }
  // Ask for free letters again; afterPanel() reloads them if the Manage tab is showing.
  S.freeLetters = null;
  loadDisks().then((changed) => { if (!changed) return; if (S.route.view === "pc") render(); else refreshOverviewBits(); });
  render();
});

/* ------------------------------------------------------------------ actions */

const actions = {
  go: (el) => go({ view: el.dataset.view }),
  "go-drive": (el) => go({ view: "drive", letter: el.dataset.letter }),
  "go-cleanup": () => go({ view: "cleanup", sub: "caches" }),
  "go-pc": () => go({ view: "pc" }),
  "tab-space": () => setTab("space"),
  "tab-optimize": () => setTab("optimize"),
  tab: (el) => setTab(el.dataset.tab),
  elevate: () => askElevate("Some tools, like trimming drives and clearing Windows Update files, need administrator rights."),
  refresh: async (el) => {
    el.disabled = true; el.innerHTML = icon("refresh-cw", "spin");
    const l = S.route.letter;
    await Promise.all([loadVolumes(), loadDisks(), loadHistory()]);
    delete S.maps[l];
    render();
    toast(`${driveName(vol(l) || { letter: l })} refreshed.`);
  },
  "open-path": (el) => invoke("open_path", { path: el.dataset.path }).catch((e) => toast(errText(e), { tone: "bad" })),
  reveal: (el) => invoke("reveal_path", { path: el.dataset.path }).catch((e) => toast(errText(e), { tone: "bad" })),
  recycle: async (el) => {
    const path = el.dataset.path, name = el.dataset.name;
    if (isRemoved(path)) return removeFromScan(path); // already recycled; the row was stale
    try {
      await invoke("recycle_paths", { paths: [path] });
      removeFromScan(path);
      toast(`Moved “${name}” to the Recycle Bin.`, { action: "Open Recycle Bin", onAction: () => invoke("open_recycle_bin") });
    } catch (e) { toast(errText(e), { tone: "bad" }); }
  },
  scan: (el) => startScan(el.dataset.path),
  "scan-folder": async () => {
    try {
      const dir = await T.dialog.open({ directory: true, multiple: false, title: "Choose a folder to scan", defaultPath: S.sys.userProfile });
      if (dir) startScan(Array.isArray(dir) ? dir[0] : dir);
    } catch (e) { toast(errText(e), { tone: "bad" }); }
  },
  "scan-cancel": () => invoke("scan_cancel"),
  // Ignore a second click (double-click) while the folder is still opening.
  drill: (el) => { if (!S.scan.opening) openNode(Number(el.dataset.id)); },
  crumb: (el) => { const i = Number(el.dataset.i); const c = S.scan.trail?.[i]; if (c) openNode(c.id, i); },
  "crumb-up": () => { const t = S.scan.trail; if (t && t.length > 1) openNode(t[t.length - 2].id, t.length - 2); },
  "space-view": (el) => { S.scan.view = el.dataset.v; renderPanel(); },
  review: (el) => startScan(el.dataset.path),
  optimize: (el) => startOptimize(S.route.letter, el.dataset.mode),
  "optimize-cancel": () => invoke("optimize_cancel"),
  check: async () => {
    const l = S.route.letter;
    if (!S.sys.elevated) return askElevate("Windows only lets administrators check a drive for errors.");
    // Redraw only the check control, so a name being typed in the same tab survives.
    const paint = () => { const c = $("#check-ctl"); const v = vol(l); if (c && v && S.route.view === "drive" && S.route.letter === l && S.route.tab === "manage") restoringFocus(() => { c.innerHTML = checkControlHTML(v); }); };
    S.checks[l] = { status: "running" }; paint();
    try { S.checks[l] = await invoke("check_volume", { letter: l }); }
    catch (e) { S.checks[l] = { error: errText(e) }; }
    paint();
  },
  "rescan-disks": async (el) => {
    el.disabled = true; el.innerHTML = `${icon("refresh-cw", "spin")}Rescanning…`;
    try { await invoke("rescan_disks"); } catch (e) { toast(errText(e), { tone: "bad" }); }
    await Promise.all([loadVolumes(), loadDisks()]);
    render();
    toast("Disk list is up to date.");
  },
  "reload-disks": async () => { S.disks = null; render(); await loadDisks(); render(); },
  "cleanup-analyze": () => { S.cleanup.lastResult = null; S.cleanup.states = {}; analyzeCleanup(); },
  "cleanup-run": () => runCleanup(),
  "clean-sub": (el) => setCleanSub(el.dataset.sub),
  "apps-reload": () => loadApps(true),
  "apps-sort": (el) => { S.apps.sort = el.dataset.v; render(); },
  orphans: () => { if (!S.apps.busy) openLeftovers(); },
  "app-uninstall": (el) => uninstallApp(el.dataset.id),
  "app-stop-wait": () => invoke("app_uninstall_stop_waiting").catch(() => {}),
  "app-leftovers": (el) => {
    const id = el.dataset.id;
    const a = S.apps.list.find((x) => x.id === id);
    openLeftovers({ appId: id, name: a?.name || (S.apps.notice?.appId === id ? S.apps.notice.name : null) });
  },
  "left-back": () => { S.apps.left = null; render({ resetScroll: true }); },
  "left-retry": () => { const L = S.apps.left; if (L) openLeftovers({ appId: L.appId, name: L.name }); },
  "left-remove": () => removeLeftovers(),
  "toggle-left": (el) => {
    const L = S.apps.left; if (!L) return;
    if (el.checked) L.sel.add(el.dataset.id); else L.sel.delete(el.dataset.id);
    const it = L.items.find((x) => x.id === el.dataset.id);
    const row = el.closest(".item");
    if (row && it) row._h = leftItemHTML(it); // the live checkbox already shows the new state
    patch($("#left-summary"), leftSummaryHTML());
  },
  "dupes-start": () => startDupes(),
  "dupes-cancel": (el) => {
    const D = S.dupes;
    if (D.status !== "running") return;
    if (D.scanId == null) D.cancelPending = true; // no scan to stop yet: startDupes stops it once dupes_start returns
    el.disabled = true; el.textContent = "Stopping…"; // the running frame is rebuilt for the next scan
    invoke("dupes_cancel").catch(() => {});
  },
  "dupes-setup": () => { S.dupes.status = "setup"; render({ resetScroll: true }); },
  "dupes-show": () => { if (S.dupes.result) { S.dupes.status = "done"; render({ resetScroll: true }); } },
  "dupes-add-root": async () => {
    const D = S.dupes;
    try {
      const dir = await T.dialog.open({ directory: true, multiple: false, title: "Choose a folder or drive to look in", defaultPath: S.sys.userProfile });
      const p = Array.isArray(dir) ? dir[0] : dir;
      if (!p) return;
      const known = [...(D.roots || []).map((r) => r.path), ...D.custom].find((x) => x.toLowerCase() === p.toLowerCase());
      if (known) { if (D.roots?.some((r) => r.path === known)) D.picked.add(known); }
      else D.custom.push(p);
      saveDupePrefs(); render();
    } catch (e) { toast(errText(e), { tone: "bad" }); }
  },
  "dupes-remove-root": (el) => { const D = S.dupes; D.custom = D.custom.filter((p) => p !== el.dataset.path); saveDupePrefs(); render(); },
  "dupes-section": (el) => { S.dupes.section = el.dataset.v; S.dupes.shown = 120; render(); },
  "dupes-more": () => { S.dupes.shown += 120; render(); },
  "dupes-recommended": () => { S.dupes.sel = recommendedDupes(); render(); },
  "dupes-clear": () => { S.dupes.sel = new Set(); render(); },
  "dupes-delete": () => deleteDupes(),
  "toggle-dupe": (el) => toggleDupe(el),
  "toggle-item": (el) => {
    const C = S.cleanup; C.touched = true;
    if (el.checked) C.selected.add(el.dataset.id); else C.selected.delete(el.dataset.id);
    // Update only what the selection affects; the checkbox (and keyboard focus) stays put.
    const it = C.items.find((x) => x.id === el.dataset.id);
    const row = el.closest(".item");
    if (row && it) row._h = itemHTML(it); // the live checkbox already shows the new state
    const sum = $("#clean-summary");
    if (sum) patch(sum, cleanSummaryHTML()); else render();
    renderRail();
  },
};

document.addEventListener("click", (e) => {
  const tb = e.target.closest("[data-theme-choice]");
  if (tb) return setTheme(tb.dataset.themeChoice);
  const el = e.target.closest("[data-action]");
  if (!el || el.disabled) return;
  const a = el.dataset.action;
  if (a === "toggle-item" || a === "toggle-left" || a === "toggle-dupe") { if (e.target !== el) return; return actions[a](el); }
  // Row action buttons sit inside clickable rows: let the innermost one win.
  e.stopPropagation();
  actions[a]?.(el, e);
});
document.addEventListener("keydown", (e) => {
  if ((e.key === "Enter" || e.key === " ") && e.target.matches(".row[data-action], tr[data-action]")) { e.preventDefault(); e.target.click(); }
  if (e.key === "Backspace" && !e.target.matches("input, select, textarea, [contenteditable]") && !$("#dialog")?.open
    && S.route.view === "drive" && S.route.tab === "space" && S.scan.status === "done" && S.scan.letter === S.route.letter && S.scan.trail) actions["crumb-up"]();
  const inField = e.target.matches?.("input, select, textarea, [contenteditable]");
  const dialogOpen = $("#dialog")?.open;
  // F5 / Ctrl+R refresh what's on screen (and stop WebView2 from reloading the whole app).
  if (e.key === "F5" || ((e.ctrlKey || e.metaKey) && !e.altKey && !e.shiftKey && e.key.toLowerCase() === "r")) {
    e.preventDefault();
    if (e.repeat || dialogOpen) return;
    if (S.route.view === "drive") $("[data-action=refresh]")?.click();
    else if (isCleanSub("caches")) $("#clean-head [data-action=cleanup-analyze]:not([disabled])")?.click();
    else if (isCleanSub("apps") && !S.apps.left && !S.apps.busy) loadApps(true);
    else if (S.route.view === "pc") refreshPC();
    return;
  }
  // 1-4 switch drive tabs.
  if (/^[1-4]$/.test(e.key) && !e.ctrlKey && !e.altKey && !e.metaKey && !inField && !dialogOpen && S.route.view === "drive" && $("#panel")) {
    const tab = TABS[Number(e.key) - 1];
    if (tab !== S.route.tab) { e.preventDefault(); setTab(tab); }
  }
});
async function refreshPC() {
  await Promise.all([loadVolumes(), loadDisks()]);
  if (S.route.view === "pc") render();
  toast("Disk list is up to date.");
}

document.addEventListener("input", (e) => {
  if (e.target.id === "apps-q") { S.apps.q = e.target.value; updateApps(); return; }
  if (e.target.id === "label-input") {
    const v = vol(S.route.letter);
    const val = e.target.value;
    if (v) { if (val === (v.label || "")) delete S.drafts.label[v.letter]; else S.drafts.label[v.letter] = val; }
    const bad = LABEL_BAD_RE.test(val);
    e.target.setAttribute("aria-invalid", String(bad));
    setFieldMsg("#label-msg", bad ? LABEL_BAD : "", bad ? "is-error" : "");
    $("#label-save").disabled = bad || val === (v?.label || "");
  }
});
document.addEventListener("change", (e) => {
  if (e.target.id === "letter-select" && S.route.letter) S.drafts.letter[S.route.letter] = e.target.value;
  if (e.target.id === "ask-admin") setAskAdmin(e.target.checked);
  const D = S.dupes;
  if (e.target.dataset?.root != null && D.picked) {
    if (e.target.checked) D.picked.add(e.target.dataset.root); else D.picked.delete(e.target.dataset.root);
    saveDupePrefs(); render();
  }
  if (e.target.dataset?.opt && D.opts) { D.opts[e.target.dataset.opt] = e.target.checked; saveDupePrefs(); }
  if (e.target.id === "dupes-min" && D.opts) { D.opts.minSize = Number(e.target.value) || DEFAULT_OPTS.minSize; saveDupePrefs(); }
});

document.addEventListener("submit", async (e) => {
  // Only the app's own forms. The confirm dialog's <form method="dialog"> must submit normally,
  // or the dialog never closes and never reports "ok".
  const f = e.target.dataset?.form;
  if (!f) return;
  e.preventDefault();
  const l = S.route.letter;
  if (f === "label") {
    const label = $("#label-input").value.trim();
    const btn = $("#label-save"); btn.disabled = true;
    try {
      await invoke("set_label", { letter: l, label });
      delete S.drafts.label[l];
      await loadVolumes(); render();
      toast(`Renamed to ${driveName(vol(l))}.`);
    } catch (err) {
      const msg = errText(err);
      if (/administrator/i.test(msg) && !S.sys.elevated) return askElevate("Windows only lets administrators rename this drive.");
      setFieldMsg("#label-msg", msg, "is-error"); if ($("#label-save")) $("#label-save").disabled = false;
    }
  }
  if (f === "letter") {
    if (!S.sys.elevated) return askElevate("Windows only lets administrators change drive letters.");
    const nl = ($("#letter-select")?.value || "").replace(":", "").toUpperCase();
    if (!/^[A-Z]$/.test(nl)) {
      setFieldMsg("#letter-msg", S.freeLetters && !S.freeLetters.length ? "Every drive letter is already in use." : "Still finding free letters. Try again in a moment.", "is-error");
      return ensureFreeLetters();
    }
    const ok = await dialog({ title: `Change ${l}: to ${nl}:?`, body: `<p>Programs installed on ${esc(l)}: and shortcuts to files there will stop finding them until you point them at ${esc(nl)}:.</p>`, confirm: `Change to ${nl}:` });
    if (!ok) return;
    try {
      await invoke("change_letter", { letter: l, newLetter: nl });
      // Anything cached under either letter now describes the wrong drive.
      forgetLetter(l); forgetLetter(nl);
      await loadVolumes(); S.freeLetters = null;
      toast(`Drive letter changed to ${nl}:.`);
      go({ view: "drive", letter: nl, tab: "manage" });
    } catch (err) { setFieldMsg("#letter-msg", errText(err), "is-error"); }
  }
});

function setFieldMsg(sel, text, cls = "") {
  const el = $(sel); if (!el) return;
  el.textContent = text; el.className = `field-msg ${cls}`;
}

// Drop per-letter caches (allocation map, check result, optimize result, typed drafts).
function forgetLetter(l) {
  delete S.maps[l]; delete S.checks[l]; delete S.opt.results[l];
  delete S.drafts.label[l]; delete S.drafts.letter[l];
}

let freeLettersLoading = false;
async function ensureFreeLetters() {
  if (S.freeLetters || freeLettersLoading) return;
  freeLettersLoading = true;
  try { S.freeLetters = await invoke("free_letters"); } catch { S.freeLetters = []; }
  freeLettersLoading = false;
  // Refresh only the picker so a name being typed on the same tab isn't disturbed.
  const sel = $("#letter-select"); const v = vol(S.route.letter);
  if (sel && v) sel.innerHTML = letterOptionsHTML(v);
}

/* ------------------------------------------------------------------ boot */

// Last-known results saved by the backend (src/cache.rs): the first paint already has disk health,
// optimization history and clean-up sizes. Fresh values replace them in the background.
function applyCache(cache) {
  const entry = (k) => (cache && cache[k] && cache[k].data != null ? cache[k] : null);
  const disks = entry("disks"), history = entry("history"), schedule = entry("schedule"), cleanup = entry("cleanup");
  if (Array.isArray(disks?.data)) S.disks = disks.data;
  if (Array.isArray(history?.data)) S.history = history.data;
  if (schedule && typeof schedule.data === "object") S.schedule = schedule.data;
  if (Array.isArray(cleanup?.data) && cleanup.data.length) {
    const C = S.cleanup;
    const savedAt = Number(cleanup.savedAt) || 0;
    const cleanedAt = Number(lsGet(CLEANED_KEY)) || 0;
    let own = null;
    try { own = JSON.parse(lsGet(OWN_KEY) || "null"); } catch {}
    // Written by this profile's last measure (savedAt is stamped just before the result returns)?
    const mine = own && Math.abs(savedAt - Number(own.doneAt)) < 15000;
    // Measured before the last clean-up started: those sizes may already be freed. Don't show them
    // (badge, capacity bar, suggestions) as reclaimable; measure again instead.
    if (cleanedAt && cleanedAt >= (mine ? Number(own.startedAt) : savedAt)) { C.stale = true; return; }
    C.items = cleanup.data;
    C.measuredAt = savedAt;
    C.stale = !mine; // measured by the other elevation level (or before this version): refresh it
    C.status = "ready";
    C.selected = new Set(C.items.filter((it) => it.defaultOn && selectable(it)).map((it) => it.id));
  }
}

// The window is shown (app_ready) right after the first frame is on screen, so it never flashes
// empty. A window created hidden produces no frames, so then it is shown as soon as the DOM is built.
let readySent = false;
function sendReady() {
  if (readySent) return;
  readySent = true;
  perf("js:app-ready");
  invoke("app_ready").catch(() => {});
  afterLaunch();
}

async function boot() {
  perf("js:boot");
  paintThemeSwitch();
  // Design-preview hooks (browser mock only): #theme=dark&view=cleanup&tab=space&scan=1&admin=1&nocache=1
  const hp = window.__PLATTER_MOCK__ ? new URLSearchParams(location.hash.slice(1)) : new URLSearchParams();
  if (hp.get("admin")) window.__PLATTER_MOCK__.mockState.elevated = true;
  if (hp.get("nocache")) window.__PLATTER_MOCK__.mockState.noCache = true;
  // Everything the first paint needs, in parallel: system facts, drives, and last-known results.
  const [sys, , cache] = await Promise.all([
    invoke("system_info").catch(() => null),
    loadVolumes(),
    invoke("cache_get_many", { keys: ["disks", "history", "schedule", "cleanup"] }).catch(() => ({})),
  ]);
  if (sys) S.sys = sys;
  perf("js:volumes");
  applyCache(cache);
  if (!S.disks) S.disksLoading = true; // list_disks starts right after the first paint
  if (S.history == null) S.historyLoading = true;
  const sysv = vol(S.sys.systemDrive) || S.volumes[0];
  S.route = savedRoute() || (sysv ? { view: "drive", letter: sysv.letter, tab: "overview" } : { view: "pc" });
  if (hp.get("theme")) setTheme(hp.get("theme"));
  if (hp.get("view") === "pc" || hp.get("view") === "cleanup") S.route = { view: hp.get("view") };
  if (hp.get("letter")) S.route.letter = hp.get("letter");
  if (hp.get("tab")) S.route.tab = hp.get("tab");
  if (hp.get("sub") && S.route.view === "cleanup") S.route.sub = hp.get("sub");
  render();
  if (S.route.view === "cleanup") { S.cleanSub = cleanSub(); if (S.cleanSub !== "caches") enterCleanSub(); }
  perf("js:first-render");
  requestAnimationFrame(() => requestAnimationFrame(() => { perf("js:first-paint"); sendReady(); }));
  // A hidden window may never produce a frame: show it once the DOM is built (with a safety net).
  if (document.visibilityState === "hidden") sendReady(); else setTimeout(sendReady, 400);
  if (hp.get("scan")) startScan(`${S.route.letter}:\\`);
  if (hp.get("files")) setTimeout(() => { S.scan.view = "files"; renderPanel(); }, 4500);
  if (hp.get("optimize")) setTimeout(() => startOptimize(S.route.letter, hp.get("optimize")), 300);
  const until = async (ok) => { for (let i = 0; i < 80 && !ok(); i++) await new Promise((r) => setTimeout(r, 100)); };
  if (hp.get("leftovers")) until(() => S.apps.list.length).then(() => { const a = S.apps.list[0]; if (a) openLeftovers({ appId: a.id, name: a.name }); });
  if (hp.get("orphans")) until(() => S.apps.status === "ready").then(() => openLeftovers());
  if (hp.get("dupes")) until(() => S.dupes.roots).then(() => startDupes());
  if (hp.get("section")) until(() => S.dupes.status === "done").then(() => { S.dupes.section = hp.get("section"); render(); });
}

// After the first paint: refresh cached data, maybe measure clean-up once idle, start polling.
function afterLaunch() {
  loadDisks().then((changed) => {
    perf("js:disks");
    if (changed) { if (S.route.view === "pc") render(); else refreshOverviewBits(); }
  });
  loadHistory().then((changed) => {
    perf("js:history");
    if (!changed || S.route.view !== "drive") return;
    if (S.route.tab === "overview") refreshOverviewBits(); else if (S.route.tab === "optimize") renderPanel();
  });
  if (isCleanSub("caches") && !cleanupFresh()) idle(measureIfStale, 500); // the user is looking at it
  else if (cleanupFresh(CLEANUP_REFRESH_MS)) perf("js:cleanup-cached");
  else { perf("js:cleanup-idle-scheduled"); scheduleIdleCleanup(3000); }
  try { if (localStorage.getItem(THEME_SYNC_KEY) !== getTheme()) syncThemePref(); } catch {}
  startPolling();
}

// Free space changes as the PC runs; keep numbers honest without disturbing the view. Nothing runs
// while the window is minimized or hidden; coming back catches up at once.
function startPolling() {
  let last = Date.now();
  const poll = async () => {
    last = Date.now();
    const key = () => JSON.stringify(S.volumes.map((v) => [v.letter, v.free, v.label, v.ready]));
    const prev = key();
    await loadVolumes();
    if (key() !== prev) {
      renderRail();
      const v = vol(S.route.letter);
      if (S.route.view === "drive" && v) patch($("#capacity"), capacityHTML(v));
    }
  };
  const tick = () => {
    if (document.hidden) return;
    poll();
    // "Measured 12 minutes ago" moves on.
    if (S.route.view === "cleanup") render(); else renderRail();
  };
  setInterval(tick, 15000);
  document.addEventListener("visibilitychange", () => { if (!document.hidden && Date.now() - last > 15000) tick(); });
}

boot().catch((e) => { console.error(e); try { render(); } catch {} sendReady(); });
