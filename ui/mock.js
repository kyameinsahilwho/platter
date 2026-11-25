// Platter — in-browser fake of the Tauri backend (see CONTRACT.md).
// Lets the vanilla-JS UI run in a plain browser for design work / screenshots.
//
//   import { installMock, mockState, mockEmit } from "./mock.js";
//   installMock();                       // no-op when a real window.__TAURI__ exists
//   mockState.elevated = true;           // flip admin on/off
//   mockState.usbPresent = false;        // unplug the USB stick (then mockEmit("drives-changed"))
//
// Backend errors are rejected as plain strings, exactly like Tauri does for Err(String).

export const mockState = {
  usbPresent: true,
  elevated: false,
  askAdmin: true, // "Ask at startup" switch (set_ask_admin)
  usbDisk: false, // also list the USB stick as physical disk 1 in list_disks
  dialogResult: "C:\\Users\\SKS\\Downloads", // what dialog.open resolves with (null = cancelled)
  noCache: false, // cache_get_many returns nothing (first launch)
  cleanupCacheAgeMin: 12, // age of the cached clean-up result (fresh under 30 minutes)
  themePref: null, // last set_theme_pref
  apps: null, // installed apps (built lazily on first apps_list; uninstalling removes one)
  uninstallCancel: false, // app_uninstall rejects as if the user declined the uninstaller / UAC
  leftoverFail: false, // leftovers_remove fails the first folder ("open in another program")
};

/* ───────────────────────────── helpers ───────────────────────────── */

const g = (x) => Math.round(x * 1e9);
const m = (x) => Math.round(x * 1e6);
const DAY = 86400000;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const rint = (a, b) => a + Math.floor(Math.random() * (b - a + 1));
const pick = (arr) => arr[Math.floor(Math.random() * arr.length)];
const iso = (ms) => new Date(ms).toISOString();
const atDay = (daysAgo, h, min) => {
  const d = new Date(Date.now() - daysAgo * DAY);
  d.setHours(h, min, 0, 0);
  return d.getTime();
};
function hashStr(s) {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return h >>> 0;
}
function mulberry32(a) {
  return () => {
    a |= 0;
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}
const normLetter = (l) => String(l ?? "").replace(/[:\\/]/g, "").toUpperCase().slice(0, 1);

const ADMIN_OPT = "Optimizing drives needs administrator rights. Restart Platter as administrator.";
const ADMIN_CHECK = "Checking for errors needs administrator rights. Restart Platter as administrator.";
const ADMIN_LETTER = "Changing a drive letter needs administrator rights. Restart Platter as administrator.";

/* ───────────────────────────── events ───────────────────────────── */

const listeners = new Map();

export function mockEmit(name, payload) {
  const set = listeners.get(name);
  if (!set) return;
  for (const cb of [...set]) {
    try {
      cb({ event: name, id: 0, payload });
    } catch (e) {
      console.error("[mock] listener for", name, "threw", e);
    }
  }
}

function listen(name, cb) {
  if (!listeners.has(name)) listeners.set(name, new Set());
  listeners.get(name).add(cb);
  return Promise.resolve(() => listeners.get(name)?.delete(cb));
}

/* ───────────────────────────── volumes ───────────────────────────── */

const vols = [
  {
    _tag: "c", letter: "C", label: "", fileSystem: "NTFS", total: 301045698560, free: 86876000000,
    kind: "fixed", isSystem: true, serial: "8A1F-33C2", clusterSize: 4096, ready: true,
    media: "ssd", bus: "NVMe", model: "INTEL SSDPEKNU512GZ", readOnly: false, compressedSupported: true,
  },
  {
    _tag: "d", letter: "D", label: "New Volume", fileSystem: "NTFS", total: 209712885760, free: 75750000000,
    kind: "fixed", isSystem: false, serial: "B04E-71D9", clusterSize: 4096, ready: true,
    media: "ssd", bus: "NVMe", model: "INTEL SSDPEKNU512GZ", readOnly: false, compressedSupported: true,
  },
  {
    _usb: true, _tag: "e", letter: "E", label: "KINGSTON", fileSystem: "exFAT", total: 30736162816, free: 12100000000,
    kind: "removable", isSystem: false, serial: "5C1A-9F02", clusterSize: 32768, ready: true,
    media: "unknown", bus: "USB", model: "Kingston DataTraveler 3.0", readOnly: false, compressedSupported: false,
  },
];
const present = (v) => !v._usb || mockState.usbPresent;
const volByTag = (t) => vols.find((v) => v._tag === t);
const volByLetter = (l) => vols.find((v) => v.letter === normLetter(l) && present(v));
const pubVol = (v) => Object.fromEntries(Object.entries(v).filter(([k]) => !k.startsWith("_")));

function needVol(letter) {
  const v = volByLetter(letter);
  if (!v) throw `Drive ${normLetter(letter)}: wasn't found.`;
  return v;
}

/* ───────────────────────────── allocation map ───────────────────────────── */

function allocationMap(letter, cellsArg) {
  const v = needVol(letter);
  const cells = Math.max(8, Math.min(20000, cellsArg | 0 || 800));
  const rnd = mulberry32(hashStr(v.letter + ":bitmap"));
  const usedFrac = 1 - v.free / v.total;
  const out = new Array(cells).fill(0);
  const fillEnd = Math.floor(cells * Math.min(0.97, usedFrac * 0.93));
  for (let i = 0; i < cells; i++) {
    const r = rnd();
    if (i < fillEnd) {
      let val = 232 + Math.floor(rnd() * 24);
      if (r < 0.14) val = 90 + Math.floor(rnd() * 140);
      if (r < 0.025) val = Math.floor(rnd() * 70);
      out[i] = val;
    } else {
      const t = (i - fillEnd) / Math.max(1, cells - fillEnd);
      out[i] = r < 0.075 * (1 - t) ? 10 + Math.floor(rnd() * 150) : 0;
    }
  }
  // MFT zone: a solid band near the start, with a slightly ragged edge
  const mftStart = Math.floor(cells * 0.055);
  const mftLen = Math.max(2, Math.floor(cells * 0.022));
  for (let i = mftStart; i < Math.min(fillEnd, mftStart + mftLen); i++) out[i] = 255;
  // a few free gaps inside the used region
  for (let k = 0; k < 4; k++) {
    const len = Math.max(1, Math.floor(cells * (0.006 + rnd() * 0.02)));
    const start = Math.floor(cells * (0.12 + rnd() * 0.7) * Math.min(1, fillEnd / cells + 0.05));
    for (let i = start; i < Math.min(fillEnd, start + len); i++) out[i] = Math.floor(rnd() * 14);
  }
  const runs = { C: 1800, D: 1460, E: 160 };
  return {
    cells: out,
    clusters: Math.floor(v.total / v.clusterSize),
    clusterSize: v.clusterSize,
    bytesPerCell: v.total / cells,
    freeRuns: runs[v.letter] ?? 900,
    largestFreeRun: Math.round(v.free * (v.letter === "C" ? 0.46 : 0.42)),
  };
}

/* ───────────────────────────── physical disks ───────────────────────────── */

function listDisks() {
  const el = mockState.elevated;
  const C = volByTag("c"), D = volByTag("d"), E = volByTag("e");
  const SIZE = 512110190592;
  let off = 1048576;
  const mk = (n, size, kind, letter, hidden, extra = {}) => {
    const p = {
      number: n, offset: off, size, letter: letter ?? null, kind,
      isSystem: kind === "EFI system", isBoot: letter === C.letter, isHidden: !!hidden, ...extra,
    };
    off += size;
    return p;
  };
  const parts = [
    mk(1, m(260), "EFI system", null, true),
    mk(2, 16 * 1048576, "Microsoft reserved", null, true),
    mk(3, C.total, "Basic data", C.letter, false),
    mk(4, m(530), "Recovery", null, true),
    mk(5, D.total, "Basic data", D.letter, false),
  ];
  // the final recovery partition takes whatever is left so the disk adds up exactly
  parts.push(mk(6, SIZE - off, "Recovery", null, true));
  const disk0 = {
    number: 0, model: "INTEL SSDPEKNU512GZ", size: SIZE, partitionStyle: "GPT", bus: "NVMe", media: "ssd",
    health: "Healthy", operational: "OK", isBoot: true, isSystem: true,
    reliabilityAvailable: el,
    temperature: el ? 41 : null, temperatureMax: el ? 70 : null, wear: el ? 3 : null,
    powerOnHours: el ? 5120 : null, readErrors: el ? 0 : null, writeErrors: el ? 0 : null,
    partitions: parts, unallocated: 0,
  };
  const out = [disk0];
  if (mockState.usbDisk && mockState.usbPresent) {
    const size = 30752563200;
    out.push({
      number: 1, model: E.model, size, partitionStyle: "MBR", bus: "USB", media: "unknown",
      health: "Healthy", operational: "OK", isBoot: false, isSystem: false,
      reliabilityAvailable: false, temperature: null, temperatureMax: null, wear: null,
      powerOnHours: null, readErrors: null, writeErrors: null,
      partitions: [{ number: 1, offset: 1048576, size: E.total, letter: E.letter, kind: "Basic data", isSystem: false, isBoot: false, isHidden: false }],
      unallocated: size - 1048576 - E.total,
    });
  }
  return out;
}

/* ───────────────────────────── optimize ───────────────────────────── */

let history = null;
function getHistory() {
  if (history) return history;
  const ev = (letter, action, t, message) => ({ letter, action, time: iso(t), ok: true, message });
  const trim = (l, t) => ev(l, "Trim", t, `Trim finished: ${l}: told the SSD which blocks are no longer in use.`);
  history = [
    trim("C", atDay(4, 1, 0)), trim("D", atDay(4, 1, 2)),
    trim("C", atDay(11, 1, 0)), trim("D", atDay(11, 1, 2)),
    trim("C", atDay(18, 1, 0)), trim("D", atDay(18, 1, 2)),
    ev("C", "Boot optimization", atDay(22, 8, 41), "Boot optimization finished: startup files on C: were placed together."),
    trim("C", atDay(25, 1, 0)),
  ].sort((a, b) => (a.time < b.time ? 1 : -1));
  return history;
}

const FRAG = { C: 2, D: 1, E: 0 };
let opt = null;

function optReport(v, mode, fragPct) {
  const gb = (b) => (b / 1073741824).toFixed(2) + " GB";
  const head = [
    "Microsoft Drive Optimizer", "Copyright (c) Microsoft Corp.", "",
    `Invoking ${mode === "trim" ? "retrim" : mode === "defrag" ? "defragmentation" : "analysis"} on ${v.label || "Local Disk"} (${v.letter}:)...`, "",
  ];
  const body = [
    `${mode === "trim" ? "Retrim" : mode === "defrag" ? "Defragmentation" : "Analysis"}:  100% complete.`, "",
    "The operation completed successfully.", "",
  ];
  if (mode === "trim") return [...head, ...body].join("\r\n");
  return [
    ...head, ...body,
    `Post ${mode === "defrag" ? "Defragmentation" : "Analysis"} Report:`, "",
    "\tVolume Information:",
    `\t\tVolume size                 = ${gb(v.total)}`,
    `\t\tFree space                  = ${gb(v.free)}`,
    `\t\tTotal fragmented space      = ${fragPct}%`,
    `\t\tLargest free space size     = ${gb(v.free * 0.46)}`,
  ].join("\r\n");
}

function optimizeStart({ letter, mode } = {}) {
  const L = normLetter(letter);
  if (!mockState.elevated) throw ADMIN_OPT;
  const v = volByLetter(L);
  if (!v) throw `Drive ${L}: wasn't found.`;
  if (!["trim", "defrag", "analyze"].includes(mode)) throw "That optimization mode isn't supported.";
  if (opt) throw "Another optimization is already running. Wait for it to finish or stop it first.";
  const stage = { trim: "Retrim", defrag: "Defragmentation", analyze: "Analysis" }[mode];
  const step = { trim: 5, defrag: 2.5, analyze: 10 }[mode];
  const run = { letter: L, mode, pct: 0, timer: null };
  opt = run;
  run.timer = setInterval(() => {
    const percent = Math.round(run.pct * 10) / 10;
    mockEmit("optimize-progress", { letter: L, mode, percent, stage, line: `${stage}: ${Math.floor(percent)}% complete.` });
    if (run.pct >= 100) {
      clearInterval(run.timer);
      opt = null;
      const frag = mode === "analyze" ? FRAG[L] ?? 1 : mode === "defrag" ? 0 : null;
      const msg = {
        trim: `Trim finished: ${L}: told the SSD which blocks are no longer in use.`,
        defrag: `Defragmentation finished: ${L}: is now tidy.`,
        analyze: `Analysis finished: ${L}: is about ${FRAG[L] ?? 1}% fragmented, so it doesn't need defragmenting.`,
      }[mode];
      getHistory().unshift({
        letter: L, action: { trim: "Trim", defrag: "Defragment", analyze: "Analyze" }[mode],
        time: iso(Date.now()), ok: true, message: msg,
      });
      mockEmit("optimize-done", { letter: L, mode, ok: true, message: msg, fragmentedPercent: frag, report: optReport(v, mode, FRAG[L] ?? 1) });
    } else {
      run.pct = Math.min(100, run.pct + step * (0.6 + Math.random() * 0.8));
    }
  }, 150);
}

function optimizeCancel() {
  if (!opt) return;
  const { letter, mode, timer } = opt;
  clearInterval(timer);
  opt = null;
  mockEmit("optimize-done", { letter, mode, ok: false, message: "Stopped before finishing.", fragmentedPercent: null, report: "" });
}

/* ───────────────────────────── fake file tree ───────────────────────────── */

const CATN = { video: 0, image: 1, audio: 2, doc: 3, arch: 4, app: 5, dev: 6, other: 7 };
const MIX = {
  video: { video: 80, other: 10, doc: 5, image: 5 },
  image: { image: 85, other: 10, doc: 5 },
  audio: { audio: 85, other: 15 },
  doc: { doc: 60, image: 15, arch: 5, other: 20 },
  arch: { arch: 80, other: 20 },
  app: { app: 75, other: 15, dev: 5, doc: 5 },
  dev: { dev: 85, other: 10, app: 5 },
  sys: { app: 80, other: 15, doc: 5 },
  mixed: { doc: 20, image: 20, video: 20, app: 15, arch: 10, other: 15 },
  cache: { other: 55, image: 15, app: 15, dev: 10, doc: 5 },
  game: { app: 45, other: 35, video: 10, audio: 10 },
};
const EXT = {
  video: "mp4 mkv mov avi webm m4v", image: "jpg jpeg png nef heic psd gif webp raw tif",
  audio: "mp3 flac wav m4a ogg", doc: "pdf doc docx ppt pptx xls xlsx txt csv",
  arch: "zip iso msi 7z rar gz tar cab", app: "exe dll sys",
  dev: "js ts rs json sql node map py",
};
const EXT_CAT = {};
for (const [k, v] of Object.entries(EXT)) for (const e of v.split(" ")) EXT_CAT[e] = CATN[k];
function catOf(name) {
  const ext = name.includes(".") ? name.split(".").pop().toLowerCase() : "";
  if (/(setup|installer)/i.test(name) && ext === "exe") return 4;
  return EXT_CAT[ext] ?? 7;
}
const dom = (cats) => {
  let best = 7, max = 0;
  cats.forEach((v, i) => { if (v > max) { max = v; best = i; } });
  return best;
};
function split(total, mixName) {
  const mx = MIX[mixName] ?? MIX.mixed;
  const w = Array(8).fill(0);
  for (const [k, v] of Object.entries(mx)) w[CATN[k]] = v;
  const sum = w.reduce((a, b) => a + b, 0);
  const out = w.map((x) => Math.floor((total * x) / sum));
  out[dom(w)] += total - out.reduce((a, b) => a + b, 0);
  return out;
}

// D(name, targetBytes, mix, { k: kids, f: [[file, bytes]], avg, rem, count, nd, denied })
const D = (n, s, mx, o = {}) => ({ n, s, mx, o });

const ALL = []; // dirs by id
let nextId = 0;
const joinP = (a, b) => (a.endsWith("\\") ? a + b : a + "\\" + b);
const pathOf = (n) => (n.parent ? joinP(pathOf(n.parent), n.name) : `${volByTag(n.tag).letter}:\\`);

function build(spec, parent, tag) {
  const node = { id: nextId++, name: spec.n, parent, tag, kids: [], files: [], other: null, nd: 0, denied: !!spec.o.denied, _a: null };
  ALL[node.id] = node;
  for (const k of spec.o.k ?? []) node.kids.push(build(k, node, tag));
  for (const [n, s] of spec.o.f ?? []) node.files.push({ name: n, size: s, cat: catOf(n) });
  let known = node.files.reduce((a, f) => a + f.size, 0);
  for (const k of node.kids) known += agg(k).size;
  const rem = spec.o.rem ?? Math.max(0, spec.s - known);
  const count = spec.o.count ?? (rem > 0 ? Math.max(1, Math.round(rem / (spec.o.avg ?? 400e3))) : 0);
  node.other = { size: rem, count, cats: split(rem, spec.mx) };
  node.nd = spec.o.nd ?? Math.floor(count / 80);
  return node;
}

function agg(n) {
  if (n._a) return n._a;
  const cats = Array(8).fill(0);
  let files = 0, dirs = n.nd;
  for (const f of n.files) { cats[f.cat] += f.size; files++; }
  if (n.other) { n.other.cats.forEach((v, i) => (cats[i] += v)); files += n.other.count; }
  for (const k of n.kids) {
    const a = agg(k);
    a.cats.forEach((v, i) => (cats[i] += v));
    files += a.files;
    dirs += a.dirs + 1;
  }
  return (n._a = { size: cats.reduce((a, b) => a + b, 0), cats, files, dirs });
}
const invalidate = () => ALL.forEach((n) => (n._a = null));

const C_SPEC = D("", 0, "sys", {
  rem: m(38), count: 14, nd: 5200,
  f: [["hiberfil.sys", g(6.4)], ["pagefile.sys", g(4.8)], ["swapfile.sys", m(256)]],
  k: [
    D("Users", g(98), "mixed", { nd: 40, k: [
      D("SKS", g(96.9), "mixed", { f: [["NTUSER.DAT", m(90)]], k: [
        D("Downloads", g(18.7), "arch", { avg: 8e6, f: [
          ["Win11_24H2_English_x64.iso", g(5.9)], ["ubuntu-24.04.1-desktop-amd64.iso", g(6.0)],
          ["Screen Recording 2026-10-02.mp4", g(2.8)], ["Wedding Highlights 4K.mp4", g(1.0)],
          ["Docker Desktop Installer.exe", m(610)], ["AndroidStudio-2025.1.1-setup.exe", m(600)],
          ["Project_Backup_Sep.zip", m(740)], ["VSCodeUserSetup-x64-1.96.2.exe", m(110)],
          ["node-v22.14.0-x64.msi", m(31)],
        ] }),
        D("AppData", g(24.1), "cache", { k: [
          D("Local", g(20.9), "cache", { k: [
            D("npm-cache", g(5.6), "dev", { avg: 30e3, nd: 3100 }),
            D("Google", g(4.9), "cache", { k: [D("Chrome", g(4.9), "cache", { k: [
              D("User Data", g(4.9), "cache", { avg: 60e3, nd: 420, f: [["History", m(212)], ["Favicons", m(96)]] }),
            ] })] }),
            D("Temp", g(3.42), "cache", { avg: 120e3, f: [
              ["AndroidStudio_setup_cache.tmp", m(612)], ["rust-installer-x86_64.msi", m(188)], ["Teams_windows_x64.exe", m(142)],
            ] }),
            D("Packages", g(3.1), "cache", { k: [
              D("MicrosoftTeams_8wekyb3d8bbwe", g(1.1), "cache"), D("5319275A.WhatsAppDesktop_cv1g1gvanyjgm", g(0.9), "cache"),
            ] }),
            D("Programs", g(1.2), "app", { k: [D("Microsoft VS Code", g(0.45), "app")] }),
            D("CrashDumps", g(1.3), "other", { f: [["chrome.exe.8124.dmp", m(512)], ["node.exe.5520.dmp", m(388)]] }),
            D("pip", g(0.73), "dev", { avg: 90e3 }),
            D("D3DSCache", g(0.41), "cache", { avg: 250e3 }),
          ] }),
          D("Roaming", g(3.2), "cache", { k: [
            D("Code", g(1.6), "cache", { k: [D("Cache", g(0.4), "cache"), D("CachedData", g(0.24), "cache"), D("User", g(0.05), "doc")] }),
            D("discord", g(0.7), "cache"), D("Microsoft", g(0.5), "doc"),
          ] }),
        ] }),
        D("Videos", g(11.2), "video", { avg: 120e6, f: [
          ["Family Trip Goa 2026.mp4", g(3.4)], ["Product Demo Final v3.mp4", g(2.1)], ["Drone_DJI_0412.mov", g(0.9)],
        ], k: [D("Captures", g(3.9), "video", { f: [
          ["Screen Recording 2026-10-14.mp4", g(1.9)], ["Screen Recording 2026-09-21.mp4", g(1.2)], ["Screen Recording 2026-08-30.mp4", m(600)],
        ] })] }),
        D("Pictures", g(5.6), "image", { avg: 3e6, f: [["Wallpaper_8K_Aurora.png", m(48)], ["panorama_ladakh_2026.jpg", m(61)]], k: [
          D("Camera Roll", g(3.1), "image", { avg: 4e6, f: [["DSC_4021.NEF", m(74)], ["DSC_4022.NEF", m(73)]] }),
          D("Screenshots", g(0.9), "image", { avg: 400e3 }), D("Saved Pictures", g(0.4), "image"),
        ] }),
        D("Documents", g(4.3), "doc", { avg: 1.2e6, f: [["dev-vm.vhdx", g(1.2)], ["Thesis_final_v7.pdf", m(94)]], k: [
          D("Work", g(1.6), "doc", { f: [["Quarterly Report FY26.pptx", m(184)], ["Client Contracts 2026.zip", m(342)]] }),
          D("Scans", g(0.9), "doc", { avg: 2e6 }),
        ] }),
        D("Projects", g(13.1), "dev", { avg: 40e3, k: [
          D("platter", g(3.1), "dev", { k: [D("target", g(2.4), "dev", { avg: 300e3 })] }),
          D("portfolio-site", g(2.6), "dev", { k: [D("node_modules", g(2.0), "dev", { avg: 18e3, nd: 2400 }), D(".next", g(0.35), "dev")] }),
          D("dashboard-app", g(3.9), "dev", { f: [["seed-dump.sql", m(410)]], k: [
            D("node_modules", g(1.9), "dev", { avg: 18e3, nd: 2100 }), D(".next", g(0.6), "dev"), D("dist", g(0.2), "dev"),
          ] }),
          D("ml-experiments", g(2.8), "dev", { k: [
            D(".venv", g(1.6), "dev", { avg: 60e3 }),
            D("datasets", g(0.8), "arch", { f: [["cifar-10-python.tar.gz", m(163)], ["train-images.csv.zip", m(388)]] }),
          ] }),
        ] }),
        D(".cargo", g(3.2), "dev", { k: [D("registry", g(2.9), "dev", { avg: 30e3, nd: 1800 }), D("bin", g(0.3), "app")] }),
        D(".rustup", g(2.4), "dev", { k: [D("toolchains", g(2.3), "dev", { avg: 250e3 })] }),
        D("OneDrive", g(4.6), "mixed", { k: [D("Documents", g(1.9), "doc"), D("Pictures", g(2.0), "image"), D("Desktop", g(0.5), "mixed")] }),
        D("Music", g(2.1), "audio", { avg: 6e6 }),
        D("Desktop", g(1.4), "mixed", { f: [["Invoice_Oct.pdf", m(2)]] }),
        D(".android", g(3.6), "app", { k: [D("avd", g(3.1), "app", { avg: 5e6 })] }),
        D(".gradle", g(1.9), "dev", { avg: 80e3 }),
        D(".vscode", g(0.6), "dev", { avg: 60e3 }),
      ] }),
      D("Public", g(0.8), "doc"),
    ] }),
    D("Windows", g(31), "sys", { avg: 200e3, nd: 6400, k: [
      D("WinSxS", g(9.8), "sys", { avg: 280e3, nd: 3200 }),
      D("System32", g(8.4), "sys", { f: [["ntoskrnl.exe", m(11)]], k: [D("DriverStore", g(3.1), "sys"), D("config", g(0.5), "other")] }),
      D("SysWOW64", g(1.1), "sys"),
      D("Installer", g(3.8), "arch", { avg: 4e6 }),
      D("SoftwareDistribution", g(4.8), "arch", { k: [D("Download", g(4.5), "arch", { avg: 6e6 })] }),
      D("Temp", m(612), "cache"),
      D("Logs", g(0.9), "other"), D("Fonts", g(0.4), "other"),
      D("ServiceProfiles", g(1.2), "cache", { k: [D("NetworkService", g(1.15), "cache", { avg: 1.5e6 })] }),
    ] }),
    D("Program Files", g(24), "app", { k: [
      D("Microsoft Visual Studio", g(6.2), "app"),
      D("Adobe", g(5.6), "app", { k: [D("Adobe Premiere Pro 2026", g(3.4), "app"), D("Adobe Photoshop 2026", g(2.2), "app")] }),
      D("Microsoft Office", g(3.0), "app"), D("Docker", g(3.4), "app"), D("JetBrains", g(2.1), "app"),
      D("Google", g(0.9), "app"), D("Common Files", g(0.7), "app"), D("Git", g(0.4), "dev"), D("nodejs", g(0.1), "dev"),
      D("WindowsApps", 0, "app", { denied: true }),
    ] }),
    D("Program Files (x86)", g(9), "app", { k: [
      D("Steam", g(3.6), "game", { k: [D("steamapps", g(3.3), "game", { avg: 2e6 })] }),
      D("Microsoft", g(3.1), "app", { k: [D("Edge", g(1.4), "app"), D("EdgeWebView", g(1.2), "app")] }),
      D("Windows Kits", g(1.1), "dev"), D("Common Files", g(0.6), "app"), D("Google", g(0.4), "app"),
    ] }),
    D("ProgramData", g(6), "sys", { k: [
      D("Package Cache", g(2.1), "arch", { avg: 5e6 }),
      D("Microsoft", g(2.2), "sys", { k: [D("Windows Defender", g(1.1), "sys"), D("Windows", g(0.7), "sys")] }),
      D("Docker", g(0.6), "app"), D("Intel", g(0.4), "sys"),
    ] }),
    D("msys64", g(3), "dev", { k: [D("usr", g(1.9), "dev"), D("mingw64", g(0.9), "dev")] }),
    D("$Recycle.Bin", g(2.1), "mixed", { k: [
      D("S-1-5-21-3512094518-2210836871-1180452733-1001", g(2.1), "mixed", { f: [
        ["$R9K2LQ4.mp4", g(0.62)], ["$R3TT0P8.iso", m(450)], ["$RXA1B77.zip", m(380)], ["$RQ77H2D.psd", m(210)],
      ] }),
    ] }),
    D("Games", g(23), "game", { avg: 3e6, k: [
      D("Epic Games", g(12.1), "game", { f: [["pakchunk0-WindowsClient.pak", g(4.2)]] }),
      D("Riot Games", g(7.9), "game"),
    ] }),
    D("Intel", g(0.5), "sys"),
    D("Recovery", 0, "sys", { denied: true }),
    D("System Volume Information", 0, "sys", { denied: true }),
  ],
});

const D_SPEC = D("", 0, "mixed", { rem: m(2900), count: 1200, nd: 90, k: [
  D("Media", g(58), "video", { k: [
    D("Movies", g(34), "video", { f: [["Concert Recording 2025.mkv", g(12.4)], ["Documentary 4K Master.mov", g(9.8)], ["Short Film Raw Cut.mp4", g(4.1)]] }),
    D("Shows", g(18), "video", { f: [["S01E01.mkv", g(2.4)], ["S01E02.mkv", g(2.3)]] }),
    D("Music", g(6), "audio", { avg: 6e6 }),
  ] }),
  D("Projects", g(31), "dev", { avg: 40e3, k: [
    D("client-portal", g(9.2), "dev", { k: [D("node_modules", g(6.8), "dev", { avg: 18e3, nd: 2800 })] }),
    D("mobile-app", g(8.4), "dev", { k: [D("build", g(3.2), "dev"), D(".gradle", g(1.4), "dev")] }),
    D("api-gateway", g(5.1), "dev"),
  ] }),
  D("Backups", g(24), "arch", { f: [["Laptop-Image-2026-09-01.zip", g(14.2)], ["photos-backup-2025.7z", g(8.1)]] }),
  D("Installers", g(9.4), "arch", { avg: 80e6, f: [["Office_2024_ProPlus.iso", g(5.2)], ["VisualStudio_Enterprise_setup.exe", m(1700)]] }),
  D("Datasets", g(8.6), "doc", { avg: 5e6, f: [["openimages-subset.tar.gz", g(3.4)]] }),
] });

const E_SPEC = D("", 0, "mixed", { rem: m(300), count: 40, k: [
  D("DCIM", g(9.8), "image", { avg: 5e6, k: [D("100CANON", g(9.8), "image", { avg: 5e6, f: [["MVI_1042.MP4", g(1.6)]] })] }),
  D("Installers", g(6.0), "arch", { f: [["ubuntu-24.04.1-desktop-amd64.iso", g(6.0)]] }),
  D("Documents", g(1.1), "doc"),
] });

const TREES = {
  c: build(C_SPEC, null, "c"), // root id 0
  d: build(D_SPEC, null, "d"),
  e: build(E_SPEC, null, "e"),
};

function findByPath(p) {
  const parts = String(p ?? "").replace(/\//g, "\\").split("\\").filter(Boolean);
  if (!parts.length) return null;
  const drive = /^([a-z]):$/i.exec(parts[0]);
  if (!drive) return null;
  const v = volByLetter(drive[1]);
  if (!v) return null;
  let dir = TREES[v._tag];
  for (let i = 1; i < parts.length; i++) {
    const low = parts[i].toLowerCase();
    const kid = dir.kids.find((k) => k.name.toLowerCase() === low);
    if (kid) { dir = kid; continue; }
    const file = i === parts.length - 1 ? dir.files.find((f) => f.name.toLowerCase() === low) : null;
    return file ? { file, parent: dir } : null;
  }
  return { dir };
}

let scanRoot = null; // dir node of the last finished scan

function dirView(n, depth) {
  const a = agg(n);
  const isScanRoot = scanRoot ? scanRoot === n : !n.parent;
  return {
    id: n.id, name: n.parent ? n.name : pathOf(n), path: pathOf(n), size: a.size, files: a.files, dirs: a.dirs,
    isDir: true, isOther: false, cat: dom(a.cats), cats: [...a.cats],
    parentId: n.parent && !isScanRoot ? n.parent.id : null, denied: n.denied,
    children: depth > 0 ? childViews(n, depth - 1) : [],
  };
}
function childViews(n, depth) {
  const out = n.kids.map((k) => dirView(k, depth));
  const base = pathOf(n);
  for (const f of n.files) {
    const cats = Array(8).fill(0);
    cats[f.cat] = f.size;
    out.push({
      id: null, name: f.name, path: joinP(base, f.name), size: f.size, files: 1, dirs: 0, isDir: false, isOther: false,
      cat: f.cat, cats, parentId: n.id, denied: false, children: [],
    });
  }
  if (n.other && n.other.count > 0 && n.other.size > 0) {
    out.push({
      id: null, name: `${n.other.count.toLocaleString("en-US")} smaller files`, path: base, size: n.other.size,
      files: n.other.count, dirs: 0, isDir: false, isOther: true, cat: dom(n.other.cats), cats: [...n.other.cats],
      parentId: n.id, denied: false, children: [],
    });
  }
  return out.sort((a, b) => b.size - a.size || a.name.localeCompare(b.name));
}

function scanNode({ id, depth } = {}) {
  const n = ALL[id];
  if (!n) throw "That folder isn't part of the last scan. Scan it again.";
  return dirView(n, Math.max(0, Math.min(3, depth | 0)));
}

function topFiles(limit) {
  const root = scanRoot ?? TREES.c;
  const out = [];
  (function walk(n) {
    const base = pathOf(n);
    for (const f of n.files) out.push({ path: joinP(base, f.name), name: f.name, size: f.size, cat: f.cat });
    n.kids.forEach(walk);
  })(root);
  return out.sort((a, b) => b.size - a.size).slice(0, Math.max(1, limit | 0 || 50));
}

function recyclePaths(paths) {
  let bytes = 0;
  for (const p of paths ?? []) {
    const hit = findByPath(p);
    if (!hit) continue;
    if (hit.file) {
      hit.parent.files.splice(hit.parent.files.indexOf(hit.file), 1);
      bytes += hit.file.size;
    } else if (hit.dir.parent) {
      bytes += agg(hit.dir).size;
      hit.dir.parent.kids.splice(hit.dir.parent.kids.indexOf(hit.dir), 1);
    }
    invalidate();
  }
  const rb = cleanup.find((c) => c.id === "recycle-bin");
  if (rb) { rb.size += bytes; rb.files += (paths ?? []).length; }
}

/* ───────────────────────────── scan ───────────────────────────── */

let scanRun = null;
const SYN = ["cache", "node_modules\\lodash", "lib", "x64", "Resources", "assets", "locales", "Cache\\Cache_Data", "logs", "bin", "Packages", "src\\components"];
const C_STATIC = [
  "C:\\Windows\\WinSxS\\amd64_microsoft-windows-shell32_31bf3856ad364e35_10.0.26100.2033_none_b1c4f7e0c3a21d9a",
  "C:\\Windows\\System32\\DriverStore\\FileRepository\\iigd_dch.inf_amd64_8c1f3a6b2d94e07c",
  "C:\\Windows\\assembly\\NativeImages_v4.0.30319_64\\System.Core",
  "C:\\ProgramData\\Package Cache\\{8d5a21f4-e5b9-4a77-8d3c-2f6e1b904a31}",
];

function scanStart({ path } = {}) {
  const hit = findByPath(path);
  if (!hit || !hit.dir) throw "Platter couldn't open that folder. Check that it exists and try again.";
  const node = hit.dir;
  if (scanRun) clearInterval(scanRun.timer);
  const pool = [];
  (function walk(n) { pool.push(pathOf(n)); n.kids.forEach(walk); })(node);
  const total = agg(node);
  const TICKS = 29;
  const run = { i: 0, timer: null, last: { files: 0, dirs: 0, bytes: 0 }, node, started: Date.now() };
  scanRun = run;
  run.timer = setInterval(() => {
    run.i++;
    const p = Math.min(1, run.i / TICKS);
    const e = 1 - Math.pow(1 - p, 1.6);
    run.last = { files: Math.floor(total.files * e), dirs: Math.floor(total.dirs * e), bytes: Math.floor(total.size * e) };
    let cur = pool[Math.floor(Math.random() * pool.length)];
    if (node === TREES.c && Math.random() < 0.18) cur = pick(C_STATIC);
    else if (Math.random() < 0.6) cur = joinP(cur, pick(SYN));
    mockEmit("scan-progress", { ...run.last, current: cur });
    if (run.i >= TICKS) {
      clearInterval(run.timer);
      scanRun = null;
      scanRoot = node;
      const a = agg(node);
      mockEmit("scan-done", {
        ok: true, cancelled: false, error: null, rootId: node.id, path: pathOf(node),
        totalBytes: a.size, files: a.files, dirs: a.dirs,
        deniedDirs: Math.max(node === TREES.c ? 37 : 0, Math.round((37 * a.size) / agg(TREES.c).size)),
        elapsedMs: Date.now() - run.started + rint(20, 140),
      });
    }
  }, 120);
}

function scanCancel() {
  if (!scanRun) return;
  const { timer, node, last, started } = scanRun;
  clearInterval(timer);
  scanRun = null;
  mockEmit("scan-done", {
    ok: false, cancelled: true, error: null, rootId: node.id, path: pathOf(node), totalBytes: last.bytes,
    files: last.files, dirs: last.dirs, deniedDirs: 0, elapsedMs: Date.now() - started,
  });
}

/* ───────────────────────────── clean up ───────────────────────────── */

const downloadsPath = "C:\\Users\\SKS\\Downloads";
// [id, group, name, description, size, files, flags]
const CLEAN_DEFS = [
  ["user-temp", "System", "Temporary files", "Scratch files apps leave behind in your user Temp folder; safe to remove.", g(3.42), 8421],
  ["windows-temp", "System", "Windows temporary files", "Short-lived files that Windows and installers leave in C:\\Windows\\Temp.", m(612), 1204, { needsAdmin: true }],
  ["recycle-bin", "System", "Recycle Bin", "Files you already deleted that still take up space until the bin is emptied.", g(2.1), 311, { kind: "recycle-bin" }],
  ["thumbnails", "System", "Thumbnail cache", "Small preview pictures Explorer keeps; Windows rebuilds them when needed.", m(284), 42],
  ["crash-dumps", "System", "Crash dumps", "Memory snapshots saved when apps crashed, only useful for debugging.", g(1.3), 27],
  ["system-error-reports", "System", "Windows error reports", "Reports and logs queued up by Windows Error Reporting.", m(96), 214, { needsAdmin: true }],
  ["windows-update", "System", "Windows Update downloads", "Installer files for updates that Windows has already applied.", g(4.8), 3187, { needsAdmin: true }],
  ["delivery-optimization", "System", "Delivery Optimization files", "Update pieces Windows cached so it could share them with other PCs.", g(1.1), 96, { needsAdmin: true }],
  ["shader-cache", "System", "Graphics shader cache", "Compiled GPU shaders that games and apps quietly rebuild.", m(410), 1840],
  ["hibernation", "System", "Hibernation file", "hiberfil.sys stores your RAM for hibernate and fast startup; turning hibernation off deletes it.", g(6.4), 1, { kind: "hibernation", needsAdmin: true, defaultOn: false }],
  ["chrome-cache", "Browsers", "Google Chrome cache", "Pages, images and scripts Chrome saved to load sites faster; your logins stay.", g(1.9), 12460],
  ["edge-cache", "Browsers", "Microsoft Edge cache", "Web content Edge saved to load sites faster; your logins stay.", m(820), 6204],
  ["firefox-cache", "Browsers", "Firefox cache", "Web content Firefox saved to load sites faster.", 0, 0, { available: false }],
  ["brave-cache", "Browsers", "Brave cache", "Web content Brave saved to load sites faster.", 0, 0, { available: false }],
  ["opera-cache", "Browsers", "Opera cache", "Web content Opera saved to load sites faster.", 0, 0, { available: false }],
  ["vscode-cache", "Apps", "Visual Studio Code cache", "Cached data and old logs that VS Code recreates as you work.", m(640), 2315],
  ["discord-cache", "Apps", "Discord cache", "Images and media Discord saved from your chats.", m(380), 1184],
  ["teams-cache", "Apps", "Microsoft Teams cache", "Temporary files Teams keeps for meetings and chats.", 0, 0, { available: false }],
  ["spotify-cache", "Apps", "Spotify cache", "Songs and artwork Spotify saved for offline-style playback.", 0, 0, { available: false }],
  ["npm-cache", "Developer caches", "npm package cache", "Packages npm downloaded earlier; it fetches again whatever it needs.", g(5.6), 41230, { defaultOn: false }],
  ["cargo-cache", "Developer caches", "Cargo registry cache", "Crate downloads and sources Cargo keeps for faster builds.", g(1.2), 18420, { defaultOn: false }],
  ["pip-cache", "Developer caches", "pip cache", "Python packages pip downloaded; it re-downloads on demand.", m(730), 2340, { defaultOn: false }],
  ["yarn-cache", "Developer caches", "Yarn cache", "Packages Yarn downloaded for offline installs.", 0, 0, { available: false }],
  ["pnpm-cache", "Developer caches", "pnpm store", "Shared package store used by pnpm.", 0, 0, { available: false }],
  ["nuget-cache", "Developer caches", "NuGet cache", "Packages NuGet downloaded for .NET projects.", 0, 0, { available: false }],
  ["gradle-cache", "Developer caches", "Gradle cache", "Dependencies and build caches Gradle keeps.", 0, 0, { available: false }],
  ["downloads", "Review", "Downloads folder", "Your Downloads folder has large files; look through them and pick what to remove.", g(18.7), 142, { kind: "review", reviewPath: downloadsPath, defaultOn: false }],
];
const cleanup = CLEAN_DEFS.map(([id, group, name, description, size, files, f = {}]) => ({
  id, group, name, description, size, files,
  needsAdmin: !!f.needsAdmin,
  defaultOn: f.defaultOn ?? f.available !== false,
  kind: f.kind ?? "delete",
  reviewPath: f.reviewPath ?? null,
  available: f.available !== false,
}));

async function cleanupAnalyze({ background = false } = {}) {
  // background: the backend measures at low priority, so it takes longer.
  const slow = background ? 2 : 1;
  for (const it of cleanup) {
    await sleep((it.available ? rint(80, 250) : rint(30, 80)) * slow);
    mockEmit("cleanup-item", { ...it });
  }
  return cleanup.map((it) => ({ ...it }));
}

async function cleanupRun({ ids } = {}) {
  const items = [];
  let freed = 0, deleted = 0, skipped = 0;
  for (const id of ids ?? []) {
    const it = cleanup.find((c) => c.id === id);
    if (!it || !it.available) { items.push({ id, freed: 0, skipped: 0, error: "That item wasn't found on this PC." }); continue; }
    if (it.kind === "review") continue;
    if (it.needsAdmin && !mockState.elevated) {
      mockEmit("cleanup-progress", { id, freed: 0, done: true });
      items.push({ id, freed: 0, skipped: 0, error: "Needs administrator rights." });
      continue;
    }
    const f = it.kind === "hibernation" ? it.size : Math.round(it.size * 0.92);
    const sk = it.kind === "hibernation" ? 0 : Math.round(it.files * 0.04);
    await sleep(200);
    mockEmit("cleanup-progress", { id, freed: Math.round(f / 2), done: false });
    await sleep(200);
    mockEmit("cleanup-progress", { id, freed: f, done: true });
    deleted += it.files - sk;
    skipped += sk;
    freed += f;
    it.size -= f;
    it.files = sk;
    items.push({ id, freed: f, skipped: sk, error: null });
  }
  const C = volByTag("c");
  C.free = Math.min(C.total, C.free + freed);
  return { freed, deleted, skipped, items };
}

/* ───────────────────────────── uninstall apps ───────────────────────────── */

const SAM = "C:\\Users\\Sam";
const APPS_RAW = [
  // [name, publisher, version, sizeMB (0 = unknown), installDate, scope, flags]
  ["7-Zip 24.08 (x64)", "Igor Pavlov", "24.08", 5.6, "2025-03-14", "machine"],
  ["Adobe Acrobat Reader", "Adobe", "24.005.20421", 612, "2025-08-02", "machine"],
  ["Adobe Creative Cloud", "Adobe Inc.", "6.4.0.361", 1480, "2025-08-02", "machine"],
  ["Audacity", "Audacity Team", "3.7.1", 98, "2025-11-09", "machine"],
  ["Battle.net", "Blizzard Entertainment", "2.43.1", 0, "2025-06-21", "machine", { broken: true }],
  ["Brave", "Brave Software, Inc.", "131.1.73.101", 640, "2026-02-17", "user"],
  ["Discord", "Discord Inc.", "1.0.9175", 395, "2026-09-30", "user"],
  ["Docker Desktop", "Docker Inc.", "4.36.0", 2860, "2026-04-18", "machine"],
  ["Figma", "Figma, Inc.", "124.7.4", 410, "2026-05-30", "user"],
  ["Git", "The Git Development Community", "2.47.1", 342, "2025-12-11", "machine"],
  ["GIMP 2.10.38", "The GIMP Team", "2.10.38", 1120, "2025-10-05", "machine"],
  ["Google Chrome", "Google LLC", "131.0.6778.205", 0, "2026-01-12", "machine"],
  ["Inkscape", "Inkscape Project", "1.4", 560, "2025-09-14", "machine"],
  ["Intel(R) Graphics Command Center", "Intel Corporation", "1.100.5001", 38, "2025-02-26", "machine"],
  ["JetBrains Toolbox", "JetBrains s.r.o.", "2.5.2.35332", 186, "2026-03-03", "user"],
  ["Logitech G HUB", "Logitech", "2024.11.667", 1240, "2025-07-19", "machine"],
  ["Microsoft Edge", "Microsoft Corporation", "131.0.2903.112", 0, "2026-02-01", "machine"],
  ["Microsoft Visual C++ 2015-2022 Redistributable (x64)", "Microsoft Corporation", "14.40.33816", 20.4, "2025-01-23", "machine"],
  ["Mozilla Firefox (x64 en-US)", "Mozilla", "133.0.3", 241, "2026-03-27", "machine"],
  ["Node.js", "Node.js Foundation", "22.12.0", 87, "2026-06-08", "machine"],
  ["Notepad++ (64-bit x64)", "Notepad++ Team", "8.7.4", 14.2, "2026-08-22", "machine"],
  ["NVIDIA Graphics Driver 566.36", "NVIDIA Corporation", "566.36", 1210, "2026-10-12", "machine"],
  ["NVIDIA GeForce Experience", "NVIDIA Corporation", "3.28.0.417", 640, "2025-04-04", "machine"],
  ["OBS Studio", "OBS Project", "30.2.3", 331, "2026-07-01", "machine"],
  ["Old Game Launcher", "Retro Works Ltd", "1.3.0", 0, "2024-12-30", "machine", { broken: true }],
  ["Opera Stable 115.0", "Opera Software", "115.0.5322.77", 0, "2026-09-02", "user"],
  ["PuTTY release 0.82 (64-bit)", "Simon Tatham", "0.82.0.0", 7.3, "2025-05-16", "machine"],
  ["Python 3.12.8 (64-bit)", "Python Software Foundation", "3.12.8150.0", 214, "2026-01-30", "user"],
  ["Slack", "Slack Technologies Inc.", "4.41.105", 372, "2026-05-12", "user"],
  ["Spotify", "Spotify AB", "1.2.52.442", 268, "2026-10-19", "user"],
  ["Steam", "Valve Corporation", "2.10.91.91", 0, "2025-03-02", "machine"],
  ["Visual Studio Code", "Microsoft Corporation", "1.96.2", 380, "2026-11-02", "user"],
  ["VLC media player", "VideoLAN", "3.0.21", 168, "2026-04-04", "machine"],
  ["WinRAR 7.01 (64-bit)", "win.rar GmbH", "7.01.0", 6.1, "2025-09-27", "machine"],
  ["Zoom Workplace (64-bit)", "Zoom Communications, Inc.", "6.2.11 (49560)", 428, "2026-08-08", "user"],
];
function buildApps() {
  return APPS_RAW.map(([name, publisher, version, mb, installDate, scope, f = {}], i) => {
    const broken = !!f.broken;
    const loc = scope === "user" ? `${SAM}\\AppData\\Local\\Programs\\${cleanName(name)}` : `C:\\Program Files\\${cleanName(name)}`;
    return {
      id: `${scope === "user" ? "HKCU" : "HKLM"}|64|app${String(i + 1).padStart(2, "0")}`,
      name, publisher, version, installDate, size: Math.round(mb * 1048576),
      installLocation: broken ? null : loc, scope, canUninstall: !broken, broken,
    };
  }).sort((a, b) => a.name.localeCompare(b.name));
}
// "Mozilla Firefox (x64 en-US)" -> "Mozilla Firefox"; "GIMP 2.10.38" -> "GIMP"
function cleanName(n) {
  return n.replace(/\s*\(.*?\)/g, "").replace(/\s+(release\s+)?v?\d+(\.\d+)+.*$/i, "").replace(/\s+Stable$/i, "").trim();
}
function getApps() {
  if (!mockState.apps) mockState.apps = buildApps();
  return mockState.apps;
}
const appsSeen = new Map(); // id -> snapshot (leftovers still work after the entry is gone)
let uninstallRun = null;

function appsList() {
  const list = getApps();
  for (const a of list) appsSeen.set(a.id, { ...a });
  return list.map((a) => ({ ...a }));
}

// A stand-in app icon: a seeded coloured tile with a simple white mark, so the preview shows
// icons offline. Broken apps and roughly one in six have none (the UI keeps the letter tile).
function appIcon({ id } = {}) {
  const app = appsSeen.get(id);
  const h = hashStr(String(id));
  if (!app || app.broken || h % 6 === 0) return null;
  const hue = h % 360, shape = (h >> 9) % 3;
  const mark = shape === 0 ? '<circle cx="32" cy="32" r="13" fill="#fff"/>'
    : shape === 1 ? '<rect x="19" y="19" width="26" height="26" rx="6" fill="#fff"/>'
    : '<path d="M32 16 48 46H16Z" fill="#fff"/>';
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><defs><linearGradient id="g" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="hsl(${hue} 70% 58%)"/><stop offset="1" stop-color="hsl(${(hue + 40) % 360} 70% 42%)"/></linearGradient></defs><rect x="4" y="4" width="56" height="56" rx="14" fill="url(#g)"/>${mark}</svg>`;
  return `data:image/svg+xml;utf8,${encodeURIComponent(svg)}`;
}

function appUninstall({ id } = {}) {
  if (uninstallRun) throw "Another uninstaller is already running.";
  const app = getApps().find((a) => a.id === id);
  if (!app) throw "That app isn't installed any more. Refresh the list and try again.";
  if (!app.canUninstall) throw "This app's uninstaller is missing. Look for leftovers to remove what's left of it.";
  return new Promise((resolve, reject) => {
    const run = { timer: null, finish: null };
    uninstallRun = run;
    run.finish = (stopped) => {
      clearTimeout(run.timer);
      uninstallRun = null;
      if (stopped) return resolve({ removed: false, exitCode: null, stoppedWaiting: true });
      if (mockState.uninstallCancel) return reject("The uninstaller was cancelled, so nothing was removed.");
      mockState.apps = getApps().filter((a) => a.id !== id);
      resolve({ removed: true, exitCode: 0, stoppedWaiting: false });
    };
    run.timer = setTimeout(() => run.finish(false), 2500);
  });
}

function appUninstallStopWaiting() {
  if (uninstallRun) uninstallRun.finish(true);
}

/* leftovers */

let leftoverSeq = 0;
let leftoverScan = null; // { scanId, app, items }
const LO_TS = "2026-11-24 101500";

function makeScan(app, items) {
  const scanId = ++leftoverSeq;
  items.forEach((it, i) => { it.id = `lo-${scanId}-${i + 1}`; });
  leftoverScan = { scanId, app, items };
  return { scanId, app, items: items.map((x) => ({ ...x })) };
}
const loItem = (kind, path, label, reason, size, confidence, needsAdmin = false) => ({
  id: "", kind, path, label, reason, size, confidence, needsAdmin, defaultOn: confidence === "high",
});

function appLeftovers({ id } = {}) {
  const app = getApps().find((a) => a.id === id) ?? appsSeen.get(id);
  if (!app) throw "That app isn't in the list any more. Refresh the list and try again.";
  const rnd = mulberry32(hashStr(app.id));
  const name = cleanName(app.name);
  const pub = (app.publisher || name).replace(/,? (Inc\.?|LLC|Ltd|GmbH|s\.r\.o\.|Corporation)$/i, "");
  const machine = app.scope === "machine";
  const mb = (a, b) => m(a + rnd() * (b - a));
  const items = [
    loItem("folder", app.installLocation ?? (machine ? `C:\\Program Files\\${name}` : `${SAM}\\AppData\\Local\\Programs\\${name}`),
      name, "In the app's install folder.", app.size || mb(20, 300), "high"),
    loItem("folder", `${SAM}\\AppData\\Roaming\\${name}`, name, "Named after the app.", mb(1, 120), "high"),
    loItem("folder", `${SAM}\\AppData\\Local\\${name}\\Cache`, "Cache", "Named after the app.", mb(8, 700), "high"),
    loItem("registry", `HKCU\\Software\\${pub}\\${name}`, name, "Settings the app saved for you.", 0, "high"),
  ];
  if (machine) items.push(loItem("registry", `HKLM\\SOFTWARE\\${pub}\\${name}`, name, "Settings the app saved for all users.", 0, "high", true));
  else items.push(loItem("folder", `${SAM}\\AppData\\Local\\${name}\\User Data`, "User Data", "Named after the app.", mb(4, 240), "high"));
  if (rnd() < 0.5) items.push(loItem("task", `\\${name} Update`, `${name} Update`, "A scheduled task named after the app.", 0, "medium", true));
  if (rnd() < 0.4) items.push(loItem("service", `${name.replace(/\W+/g, "")}UpdateSvc`, `${name} Update Service`, "A service that runs the app's program file.", 0, "medium", true));
  if (rnd() < 0.5) items.push(loItem("startup", `HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run \u2192 ${name}`, name, "Starts the app when you sign in.", 0, "high"));
  if (machine && rnd() < 0.75) items.push(loItem("folder", `C:\\ProgramData\\${name}`, name, "Might belong to this app; check before removing.", mb(0.5, 60), "low"));
  return makeScan(app.name, items);
}

function orphanLeftovers() {
  const items = [
    loItem("uninstall-entry", "HKLM\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\OldGameLauncher_is1", "Old Game Launcher", "Its program and install folder are gone.", 0, "high", true),
    loItem("service", "AdobeARMservice", "Adobe Acrobat Update Service", "Its program file is gone.", 0, "medium", true),
    loItem("task", "\\GoogleUpdateTaskMachineCore", "GoogleUpdateTaskMachineCore", "Its program file is gone.", 0, "medium", true),
    loItem("startup", "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run \u2192 OneDriveSetup", "OneDriveSetup", "Its program file is gone.", 0, "high"),
    loItem("folder", "C:\\Program Files (x86)\\Retro Works", "Retro Works", "No programs inside and nothing changed in 90 days.", m(412), "medium", true),
    loItem("folder", "C:\\ProgramData\\Old Game Launcher", "Old Game Launcher", "No programs inside and nothing changed in 90 days.", m(86), "medium", true),
    loItem("folder", `${SAM}\\AppData\\Local\\Programs\\Skype`, "Skype", "No programs inside and nothing changed in 90 days.", m(23), "medium"),
    loItem("registry", "HKLM\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\oldlauncher.exe", "oldlauncher.exe", "It points at a file that no longer exists.", 0, "medium", true),
  ];
  for (const it of items) if (it.kind === "folder") it.defaultOn = false;
  return makeScan(null, items);
}

function leftoversRemove({ scanId, ids } = {}) {
  if (!leftoverScan || leftoverScan.scanId !== scanId) throw "These results are out of date. Look for leftovers again.";
  const out = [];
  let removed = 0, failed = 0, freed = 0, backup = false;
  let failUsed = false;
  for (const id of ids ?? []) {
    const it = leftoverScan.items.find((x) => x.id === id);
    if (!it) { out.push({ id, ok: false, error: "That item is already gone." }); failed++; continue; }
    if (it.needsAdmin && !mockState.elevated) {
      out.push({ id, ok: false, error: "Needs administrator rights. Restart Platter as administrator and try again." });
      failed++;
      continue;
    }
    if (mockState.leftoverFail && !failUsed && it.kind === "folder") {
      failUsed = true;
      out.push({ id, ok: false, error: "This folder is open in another program. Close it and try again." });
      failed++;
      continue;
    }
    out.push({ id, ok: true, error: null });
    removed++;
    freed += it.size;
    if (it.kind !== "folder" && it.kind !== "file") backup = true;
    leftoverScan.items = leftoverScan.items.filter((x) => x !== it);
  }
  return {
    removed, failed, freed, items: out,
    backupFolder: backup ? `${SAM}\\AppData\\Local\\app.platter.disks\\backups\\${LO_TS} ${leftoverScan.app ?? "Orphans"}` : null,
  };
}

/* ───────────────────────────── duplicates ───────────────────────────── */

const DUPE_ROOTS = ["Desktop", "Documents", "Downloads", "Pictures", "Videos", "Music"].map((l) => ({ path: `${SAM}\\${l}`, label: l }));
// folders the recommendation rules treat as "app / project" (package.json, .exe+.dll, ...)
const PROT_ROOTS = [`${SAM}\\Documents\\Projects\\portfolio-site`, `${SAM}\\Documents\\Backups\\portfolio-site backup`, `${SAM}\\Downloads\\Blender 4.2 portable`, `${SAM}\\Documents\\Projects\\dashboard-app`];
const isProt = (p) => /\\(Windows|Program Files( \(x86\))?|ProgramData|AppData)\\/i.test(p) || PROT_ROOTS.some((r) => p.toLowerCase().startsWith(r.toLowerCase() + "\\") || p.toLowerCase() === r.toLowerCase());
const isCopyName = (n) => / - Copy|\(\d+\)|^Copy of /i.test(n);
const dirOf = (p) => p.slice(0, p.lastIndexOf("\\"));
const baseOf = (p) => p.slice(p.lastIndexOf("\\") + 1);
function rankOf(p) {
  if (isProt(p)) return 0;
  if (/\\(Pictures|Documents|Videos|Music)(\\|$)/i.test(p)) return 1;
  if (/\\Desktop(\\|$)/i.test(p)) return 2;
  if (/\\(Downloads|Temp)(\\|$)/i.test(p)) return 4;
  return 3;
}
// top-level user folder of a path, for notes ("Pictures", "Downloads")
function folderWhere(p) {
  const f = dirOf(p).replace(SAM + "\\", "");
  return f.split("\\")[0];
}

// copies: [{ path, size, daysAgo }] -> DupFile[] following the contract's recommendation rules
function recommend(copies, selectable = true) {
  const files = copies.map((c) => ({
    path: c.path, name: baseOf(c.path), folder: dirOf(c.path), size: c.size,
    modified: Date.now() - c.daysAgo * DAY, keep: false, select: false, risk: "review", note: "",
  }));
  files.sort((a, b) =>
    rankOf(a.path) - rankOf(b.path) ||
    Number(isCopyName(a.name)) - Number(isCopyName(b.name)) ||
    a.modified - b.modified || a.path.length - b.path.length);
  const keeper = files[0];
  keeper.keep = true;
  const oldest = Math.min(...files.map((x) => x.modified));
  for (const f of files) {
    const where = folderWhere(f.path);
    if (isProt(f.path)) { f.risk = "protected"; f.note = "Part of an app's folder, so leave it"; }
    else if (/\\(Downloads|Temp)(\\|$)/i.test(f.path)) { f.risk = "safe"; f.note = `Copy in ${where}`; }
    else if (isCopyName(f.name)) { f.risk = "safe"; f.note = "Named like a copy"; }
    else if (f.folder === keeper.folder) { f.risk = "safe"; f.note = "Same folder as the copy we keep"; }
    else { f.risk = "review"; f.note = `Also in ${where}`; }
    if (f.keep && f.risk !== "protected") f.note = `${f.modified === oldest ? "Oldest copy" : "Best copy"}, in ${where}`;
    f.select = selectable && !f.keep && f.risk !== "protected";
  }
  return files;
}

function buildDupes(scan) {
  const rnd = mulberry32(hashStr("dupes-v1"));
  const ri = (a, b) => a + Math.floor(rnd() * (b - a + 1));
  const o = scan.options;
  const exact = [];
  let gid = 0;
  const addGroup = (copies) => {
    const files = recommend(copies);
    const size = copies[0].size;
    exact.push({
      id: `g${++gid}`, cat: catOf(files[0].name), size,
      reclaimable: size * files.filter((f) => f.select).length, files,
    });
  };
  // photos duplicated between Pictures and Downloads / " - Copy" names
  for (let i = 0; i < 14; i++) {
    const n = 2000 + ri(0, 7999);
    const ext = ["jpg", "jpg", "heic", "png"][ri(0, 3)];
    const size = ri(1800, 6200) * 1024;
    const d = ri(40, 900);
    const c = [{ path: `${SAM}\\Pictures\\Camera Roll\\IMG_${n}.${ext}`, size, daysAgo: d }];
    if (i % 3 === 0) c.push({ path: `${SAM}\\Pictures\\Camera Roll\\IMG_${n} - Copy.${ext}`, size, daysAgo: d - 3 });
    else c.push({ path: `${SAM}\\Downloads\\IMG_${n}.${ext}`, size, daysAgo: d - ri(1, 30) });
    if (i % 5 === 0) c.push({ path: `${SAM}\\Desktop\\IMG_${n}.${ext}`, size, daysAgo: d - 10 });
    addGroup(c);
  }
  // videos
  [["Family Trip Goa 2026", "mp4", 842], ["Product Demo Final v3", "mp4", 410], ["Drone_DJI_0412", "mov", 296], ["Screen Recording 2026-09-21", "mp4", 188], ["Wedding Highlights 4K", "mp4", 655]].forEach(([n, ext, mb], i) => {
    addGroup([
      { path: `${SAM}\\Videos\\${n}.${ext}`, size: m(mb), daysAgo: 120 + i * 20 },
      { path: `${SAM}\\Downloads\\${n} (1).${ext}`, size: m(mb), daysAgo: 60 + i * 5 },
    ]);
  });
  // installers in Downloads x2-3
  [["vlc-3.0.21-win64", "exe", 42], ["7z2408-x64", "exe", 1.6], ["node-v22.12.0-x64", "msi", 31], ["VSCodeUserSetup-x64-1.96.2", "exe", 110],
   ["Docker Desktop Installer", "exe", 610], ["python-3.12.8-amd64", "exe", 27], ["GIMP-2.10.38-setup", "exe", 316], ["obs-studio-30.2.3-windows", "zip", 118]].forEach(([n, ext, mb], i) => {
    const c = [
      { path: `${SAM}\\Downloads\\${n}.${ext}`, size: m(mb), daysAgo: 30 + i * 9 },
      { path: `${SAM}\\Downloads\\${n} (1).${ext}`, size: m(mb), daysAgo: 20 + i * 9 },
    ];
    if (i % 2 === 0) c.push({ path: `${SAM}\\Downloads\\Old\\${n}.${ext}`, size: m(mb), daysAgo: 200 + i });
    addGroup(c);
  });
  // PDFs
  ["Invoice_Oct", "Thesis_final_v7", "Lease Agreement 2026", "Tax Return 2025", "Client Contract Acme", "Resume Sam 2026", "Insurance Policy", "Quarterly Report FY26"].forEach((n, i) => {
    const size = ri(90, 4800) * 1024;
    const c = [
      { path: `${SAM}\\Documents\\${i % 2 ? "Work" : "Invoices"}\\${n}.pdf`, size, daysAgo: 150 + i * 11 },
      { path: `${SAM}\\Downloads\\${n}.pdf`, size, daysAgo: 100 + i * 7 },
    ];
    if (i % 3 === 0) c.push({ path: `${SAM}\\Desktop\\${n}.pdf`, size, daysAgo: 90 });
    addGroup(c);
  });
  // protected copies inside project / app folders
  addGroup([
    { path: `${SAM}\\Documents\\Projects\\portfolio-site\\public\\img\\hero.jpg`, size: m(3.1), daysAgo: 240 },
    { path: `${SAM}\\Pictures\\hero.jpg`, size: m(3.1), daysAgo: 200 },
  ]);
  addGroup([
    { path: `${SAM}\\Documents\\Projects\\portfolio-site\\public\\logo.png`, size: 184320, daysAgo: 240 },
    { path: `${SAM}\\Documents\\Projects\\portfolio-site\\src\\assets\\logo.png`, size: 184320, daysAgo: 238 },
  ]);
  addGroup([
    { path: `${SAM}\\Documents\\Projects\\dashboard-app\\seed\\seed-dump.sql`, size: m(410), daysAgo: 70 },
    { path: `${SAM}\\Documents\\Projects\\dashboard-app\\backup\\seed-dump.sql`, size: m(410), daysAgo: 71 },
  ]);
  addGroup([
    { path: `${SAM}\\Downloads\\Blender 4.2 portable\\4.2\\python\\bin\\python311.dll`, size: m(5.8), daysAgo: 55 },
    { path: `${SAM}\\Downloads\\Blender 4.2 portable\\blender.shared\\python311.dll`, size: m(5.8), daysAgo: 55 },
  ]);
  addGroup([
    { path: `${SAM}\\Documents\\Projects\\portfolio-site\\docs\\spec.pdf`, size: m(1.4), daysAgo: 160 },
    { path: `${SAM}\\Documents\\Work\\spec.pdf`, size: m(1.4), daysAgo: 150 },
    { path: `${SAM}\\Downloads\\spec.pdf`, size: m(1.4), daysAgo: 149 },
  ]);
  const minSize = o.minSize | 0;
  const groups = exact.filter((x) => x.size >= minSize).sort((a, b) => b.reclaimable - a.reclaimable);

  // folder groups
  const mkFolder = (id, size, files, copies) => {
    const f = recommend(copies.map((c) => ({ ...c, size })));
    return { id, size, files, reclaimable: size * f.filter((x) => x.select).length, folders: f };
  };
  const folders = !o.folders ? [] : [
    mkFolder("f1", m(412), 48, [
      { path: `${SAM}\\Pictures\\Holiday 2024`, daysAgo: 360 },
      { path: `${SAM}\\Downloads\\Holiday 2024 (1)`, daysAgo: 330 },
    ]),
    mkFolder("f2", m(96), 1840, [
      { path: `${SAM}\\Documents\\Projects\\portfolio-site`, daysAgo: 200 },
      { path: `${SAM}\\Documents\\Backups\\portfolio-site backup`, daysAgo: 120 },
    ]),
    mkFolder("f3", m(238), 31, [
      { path: `${SAM}\\Music\\Playlists\\Road Trip`, daysAgo: 400 },
      { path: `${SAM}\\Desktop\\Copy of Road Trip`, daysAgo: 90 },
    ]),
  ].sort((a, b) => b.reclaimable - a.reclaimable);

  // similar images / videos (never selected: similar isn't identical)
  const similar = (kind, id, specs) => {
    const files = recommend(specs.map((s) => ({ path: s.path, size: s.size, daysAgo: s.daysAgo })), false).map((f) => {
      const s = specs.find((x) => x.path === f.path);
      return { ...f, width: s.w, height: s.h, durationMs: kind === "video" ? s.dur : null, similarity: f.keep ? 100 : s.sim };
    });
    return { id, kind, files };
  };
  const images = !o.similarImages ? [] : [
    similar("image", "i1", [
      { path: `${SAM}\\Pictures\\Camera Roll\\DSC_4021.jpg`, size: m(7.2), daysAgo: 300, w: 6000, h: 4000, sim: 100 },
      { path: `${SAM}\\Pictures\\Saved Pictures\\DSC_4021_edit.jpg`, size: m(2.4), daysAgo: 280, w: 3000, h: 2000, sim: 96 },
      { path: `${SAM}\\Downloads\\DSC_4021_small.jpg`, size: 412000, daysAgo: 250, w: 1200, h: 800, sim: 92 },
    ]),
    similar("image", "i2", [
      { path: `${SAM}\\Pictures\\panorama_ladakh_2026.jpg`, size: m(61), daysAgo: 90, w: 16384, h: 3200, sim: 100 },
      { path: `${SAM}\\Desktop\\panorama_ladakh_preview.jpg`, size: m(1.8), daysAgo: 85, w: 4096, h: 800, sim: 97 },
    ]),
    similar("image", "i3", [
      { path: `${SAM}\\Pictures\\Screenshots\\Screenshot 2026-10-01 101200.png`, size: m(1.1), daysAgo: 54, w: 1920, h: 1080, sim: 100 },
      { path: `${SAM}\\Pictures\\Screenshots\\Screenshot 2026-10-01 101205.png`, size: m(1.1), daysAgo: 54, w: 1920, h: 1080, sim: 95 },
      { path: `${SAM}\\Downloads\\Screenshot 2026-10-01 101210.png`, size: m(1.0), daysAgo: 54, w: 1920, h: 1080, sim: 90 },
      { path: `${SAM}\\Desktop\\Screenshot 2026-10-01 101215.png`, size: m(1.0), daysAgo: 53, w: 1920, h: 1080, sim: 88 },
    ]),
    similar("image", "i4", [
      { path: `${SAM}\\Pictures\\Holiday 2024\\beach_sunset.jpg`, size: m(4.4), daysAgo: 360, w: 4032, h: 3024, sim: 100 },
      { path: `${SAM}\\Pictures\\Holiday 2024\\beach_sunset_2.jpg`, size: m(4.3), daysAgo: 360, w: 4032, h: 3024, sim: 94 },
    ]),
    similar("image", "i5", [
      { path: `${SAM}\\Pictures\\Wallpaper_8K_Aurora.png`, size: m(48), daysAgo: 400, w: 7680, h: 4320, sim: 100 },
      { path: `${SAM}\\Downloads\\Wallpaper_Aurora_4K.jpg`, size: m(3.6), daysAgo: 399, w: 3840, h: 2160, sim: 93 },
      { path: `${SAM}\\Desktop\\Wallpaper_Aurora_HD.jpg`, size: m(0.9), daysAgo: 398, w: 1920, h: 1080, sim: 89 },
    ]),
    similar("image", "i6", [
      { path: `${SAM}\\Documents\\Work\\team-photo.jpg`, size: m(2.8), daysAgo: 180, w: 3000, h: 2000, sim: 100 },
      { path: `${SAM}\\Downloads\\team-photo-WhatsApp.jpg`, size: 286000, daysAgo: 178, w: 1600, h: 1067, sim: 91 },
    ]),
  ];
  const videos = !o.similarVideos ? [] : [
    similar("video", "v1", [
      { path: `${SAM}\\Videos\\Captures\\Screen Recording 2026-10-14.mp4`, size: g(1.9), daysAgo: 41, w: 2560, h: 1440, dur: 1284000, sim: 100 },
      { path: `${SAM}\\Downloads\\Screen Recording 2026-10-14 compressed.mp4`, size: m(310), daysAgo: 40, w: 1920, h: 1080, dur: 1284000, sim: 94 },
    ]),
    similar("video", "v2", [
      { path: `${SAM}\\Videos\\Family Trip Goa 2026 master.mp4`, size: g(3.4), daysAgo: 120, w: 3840, h: 2160, dur: 612000, sim: 100 },
      { path: `${SAM}\\Desktop\\Goa trip WhatsApp.mp4`, size: m(48), daysAgo: 118, w: 1280, h: 720, dur: 611000, sim: 90 },
      { path: `${SAM}\\Downloads\\Family Trip Goa 2026 (720p).mp4`, size: m(540), daysAgo: 119, w: 1280, h: 720, dur: 612000, sim: 96 },
    ]),
  ];

  const reclaimable = groups.reduce((a, x) => a + x.reclaimable, 0) + folders.reduce((a, x) => a + x.reclaimable, 0);
  return {
    scanId: scan.scanId, roots: scan.roots, files: 48213, bytes: g(212.4),
    skipped: { denied: 4, cloud: 126, small: minSize > 0 ? 9874 : 0 },
    reclaimable, groups, folders, images, videos,
  };
}

let dupesSeq = 0;
let dupesRun = null; // { scanId, timer }
let dupesLatest = null; // { scanId, roots, options, result }

const PHASE_WEIGHT = { listing: 1.2, hashing: 2.2, images: 0.8, videos: 0.5, folders: 0.3 };
const PHASE_TOTAL = { listing: 0, hashing: 6420, images: 1380, videos: 64, folders: 912 };
const DUPE_SAMPLE = ["Pictures\\Camera Roll\\IMG_%.jpg", "Downloads\\setup-%.exe", "Documents\\Work\\report_%.pdf", "Videos\\Captures\\clip_%.mp4", "Music\\Albums\\track_%.mp3", "Desktop\\notes_%.txt"];

function dupesStart({ roots, options } = {}) {
  if (!roots?.length) throw "Choose at least one folder to scan.";
  if (dupesRun) dupesCancel();
  const o = { minSize: 0, similarImages: false, similarVideos: false, folders: false, ...(options ?? {}) };
  const scanId = ++dupesSeq;
  const phases = ["listing", "hashing"];
  if (o.similarImages) phases.push("images");
  if (o.similarVideos) phases.push("videos");
  if (o.folders) phases.push("folders");
  const weights = phases.map((p) => PHASE_WEIGHT[p]);
  const sum = weights.reduce((a, b) => a + b, 0);
  const TOTAL_MS = 5000;
  const started = Date.now();
  const run = { scanId, timer: null };
  dupesRun = run;
  run.timer = setInterval(() => {
    const t = Math.min(1, (Date.now() - started) / TOTAL_MS);
    let acc = 0, idx = phases.length - 1, within = 1;
    for (let i = 0; i < phases.length; i++) {
      const w = weights[i] / sum;
      if (t <= acc + w || i === phases.length - 1) { idx = i; within = Math.min(1, (t - acc) / w); break; }
      acc += w;
    }
    const phase = phases[idx];
    const total = PHASE_TOTAL[phase];
    const files = Math.floor(48213 * (idx === 0 ? within : 1));
    const bytes = Math.floor(g(212.4) * (idx === 0 ? within : 1));
    const done = phase === "listing" ? files : Math.floor(total * within);
    mockEmit("dupes-progress", {
      scanId, phase, files, bytes, done, total,
      current: `${SAM}\\${pick(DUPE_SAMPLE).replace("%", String(rint(1000, 9999)))}`,
    });
    if (t >= 1) {
      clearInterval(run.timer);
      dupesRun = null;
      dupesLatest = { scanId, roots: [...roots], options: o, result: null };
      mockEmit("dupes-done", { scanId, ok: true, cancelled: false, error: null, elapsedMs: Date.now() - started });
    }
  }, 150);
  return scanId;
}

function dupesCancel() {
  if (!dupesRun) return;
  const { timer, scanId } = dupesRun;
  clearInterval(timer);
  dupesRun = null;
  mockEmit("dupes-done", { scanId, ok: false, cancelled: true, error: null, elapsedMs: 0 });
}

function dupesResult({ scanId } = {}) {
  if (!dupesLatest || dupesLatest.scanId !== scanId) throw "These results are out of date. Scan again.";
  if (!dupesLatest.result) dupesLatest.result = buildDupes(dupesLatest);
  return structuredClone(dupesLatest.result);
}

function dupesThumb({ path } = {}) {
  let seed = hashStr(String(path));
  const r = dupesLatest?.result;
  if (r) {
    const grp = [...r.groups, ...r.images, ...r.videos].find((x) => x.files.some((f) => f.path === path));
    if (grp) seed = hashStr(grp.id); // same picture for every copy / similar file of a group
  }
  const rnd = mulberry32(seed);
  const hue = Math.floor(rnd() * 360), hue2 = (hue + 40 + Math.floor(rnd() * 80)) % 360;
  const shape = Math.floor(rnd() * 3);
  const cx = 20 + Math.floor(rnd() * 56), cy = 20 + Math.floor(rnd() * 56), rr = 12 + Math.floor(rnd() * 22);
  const fill = `hsl(${hue2} 80% 75%)`;
  const sh = shape === 0 ? `<circle cx="${cx}" cy="${cy}" r="${rr}" fill="${fill}"/>`
    : shape === 1 ? `<rect x="${cx - rr}" y="${cy - rr}" width="${rr * 2}" height="${Math.round(rr * 1.4)}" rx="6" fill="${fill}"/>`
    : `<polygon points="${cx},${cy - rr} ${cx + rr},${cy + rr} ${cx - rr},${cy + rr}" fill="${fill}"/>`;
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="96" height="96" viewBox="0 0 96 96"><defs><linearGradient id="g" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="hsl(${hue} 65% 52%)"/><stop offset="1" stop-color="hsl(${hue2} 70% 38%)"/></linearGradient></defs><rect width="96" height="96" fill="url(#g)"/>${sh}</svg>`;
  return "data:image/svg+xml;utf8," + encodeURIComponent(svg);
}

function dupesDelete({ scanId, paths } = {}) {
  if (!dupesLatest || dupesLatest.scanId !== scanId || !dupesLatest.result) throw "These results are out of date. Scan again.";
  const r = dupesLatest.result;
  // Like the backend: a copy is removed when it is named, or sits inside a named folder copy.
  const named = [...new Set((paths ?? []).map((p) => p.toLowerCase()))];
  const under = (l) => named.some((d) => l.startsWith(d.replace(/\\+$/, "") + "\\"));
  const del = { has: (p) => named.includes(p.toLowerCase()) || under(p.toLowerCase()) };
  const counted = (p) => named.includes(p.toLowerCase()) && !under(p.toLowerCase()); // not gone with its folder
  // a uniform view: every kind of group exposes its copies as `list` plus the size of one copy
  const views = [
    ...r.groups.map((x) => ({ list: () => x.files, set: (l) => (x.files = l), size: () => x.size })),
    ...r.folders.map((x) => ({ list: () => x.folders, set: (l) => (x.folders = l), size: () => x.size })),
    ...[...r.images, ...r.videos].map((x) => ({ list: () => x.files, set: (l) => (x.files = l), size: null })),
  ];
  for (const v of views) {
    const l = v.list();
    if (l.length && l.every((f) => del.has(f.path))) {
      throw "Platter keeps at least one copy of every file, so it left these alone. Keep one copy in each group and try again.";
    }
  }
  let removed = 0, freed = 0;
  for (const v of views) {
    const gone = v.list().filter((f) => del.has(f.path));
    if (!gone.length) continue;
    for (const f of gone) if (counted(f.path)) { removed++; freed += v.size ? v.size() : f.size; }
    const rest = v.list().filter((f) => !del.has(f.path));
    if (rest.length && !rest.some((f) => f.keep)) { rest[0].keep = true; rest[0].select = false; }
    v.set(rest);
  }
  r.groups = r.groups.filter((x) => x.files.length >= 2);
  r.folders = r.folders.filter((x) => x.folders.length >= 2);
  r.images = r.images.filter((x) => x.files.length >= 2);
  r.videos = r.videos.filter((x) => x.files.length >= 2);
  for (const x of r.groups) x.reclaimable = x.size * x.files.filter((f) => f.select).length;
  for (const x of r.folders) x.reclaimable = x.size * x.folders.filter((f) => f.select).length;
  r.reclaimable = r.groups.reduce((a, x) => a + x.reclaimable, 0) + r.folders.reduce((a, x) => a + x.reclaimable, 0);
  return { removed, freed, failed: [] };
}

/* ───────────────────────────── command table ───────────────────────────── */

const handlers = {
  system_info: () => ({
    elevated: mockState.elevated, askAdminAtStart: mockState.askAdmin, computerName: "SKS-LAPTOP", systemDrive: "C",
    userProfile: "C:\\Users\\SKS", downloads: downloadsPath,
  }),
  relaunch_elevated: () => { mockState.elevated = true; return true; },
  set_ask_admin: ({ enabled } = {}) => { mockState.askAdmin = !!enabled; },

  list_volumes: () => vols.filter(present).map(pubVol),
  allocation_map: ({ letter, cells } = {}) => allocationMap(letter, cells),

  list_disks: () => listDisks(),
  rescan_disks: () => undefined,

  optimize_start: (a) => optimizeStart(a),
  optimize_cancel: () => optimizeCancel(),
  optimize_history: () => getHistory().map((e) => ({ ...e })),
  optimize_schedule: () => ({ enabled: true, lastRun: iso(atDay(4, 1, 0)), nextRun: iso(atDay(-3, 1, 0)) }),

  set_label: ({ letter, label } = {}) => {
    const v = needVol(letter);
    label = String(label ?? "");
    if (label.length > 32) throw "Volume names can be at most 32 characters.";
    v.label = label;
  },
  free_letters: () => {
    const used = new Set(vols.filter(present).map((v) => v.letter));
    return "DEFGHIJKLMNOPQRSTUVWXYZ".split("").filter((l) => !used.has(l));
  },
  change_letter: ({ letter, newLetter } = {}) => {
    const v = needVol(letter);
    if (v.isSystem) throw `Windows is installed on ${v.letter}:, so its letter can't be changed.`;
    if (!mockState.elevated) throw ADMIN_LETTER;
    const nl = normLetter(newLetter);
    if (!/^[D-Z]$/.test(nl)) throw "Choose a drive letter between D and Z.";
    if (vols.some((x) => present(x) && x.letter === nl)) throw `Drive letter ${nl}: is already in use.`;
    v.letter = nl;
    setTimeout(() => mockEmit("drives-changed"), 400);
  },
  check_volume: ({ letter } = {}) => {
    const v = needVol(letter);
    if (!mockState.elevated) throw ADMIN_CHECK;
    return { status: "NoErrorsFound", message: `Windows scanned ${v.letter}: and found no problems.` };
  },
  open_path: ({ path } = {}) => console.info("[mock] open_path", path),
  reveal_path: ({ path } = {}) => console.info("[mock] reveal_path", path),
  open_recycle_bin: () => console.info("[mock] open_recycle_bin"),
  recycle_paths: ({ paths } = {}) => recyclePaths(paths),

  scan_start: (a) => scanStart(a),
  scan_cancel: () => scanCancel(),
  scan_node: (a) => scanNode(a),
  scan_top_files: ({ limit } = {}) => topFiles(limit),

  cleanup_analyze: (a) => cleanupAnalyze(a),
  cleanup_run: (a) => cleanupRun(a),

  // startup: last-known results (src/cache.rs), window show, theme, perf timeline
  cache_get_many: ({ keys } = {}) => cacheGetMany(keys),
  app_ready: () => undefined,
  set_theme_pref: ({ theme } = {}) => { mockState.themePref = theme; },
  perf_mark: () => undefined,
  perf_enabled: () => false,

  apps_list: () => appsList(),
  app_icon: (a) => appIcon(a),
  app_uninstall: (a) => appUninstall(a),
  app_uninstall_stop_waiting: () => appUninstallStopWaiting(),
  app_leftovers: (a) => appLeftovers(a),
  orphan_leftovers: () => orphanLeftovers(),
  leftovers_remove: (a) => leftoversRemove(a),

  dupes_default_roots: () => DUPE_ROOTS.map((r) => ({ ...r })),
  dupes_start: (a) => dupesStart(a),
  dupes_cancel: () => dupesCancel(),
  dupes_result: (a) => dupesResult(a),
  dupes_thumb: (a) => dupesThumb(a),
  dupes_delete: (a) => dupesDelete(a),
};

function cacheGetMany(keys = []) {
  if (mockState.noCache) return {};
  const now = Date.now();
  const entry = (minutesAgo, data) => ({ savedAt: now - minutesAgo * 60000, version: 1, data: structuredClone(data) });
  const all = {
    disks: () => entry(95, listDisks()),
    history: () => entry(95, getHistory()),
    schedule: () => entry(95, handlers.optimize_schedule()),
    cleanup: () => entry(mockState.cleanupCacheAgeMin, cleanup),
  };
  return Object.fromEntries(keys.filter((k) => all[k]).map((k) => [k, all[k]()]));
}

const DELAYS = {
  list_disks: 900, allocation_map: 300, rescan_disks: 600, check_volume: 1400, relaunch_elevated: 700,
  cleanup_analyze: 0, cleanup_run: 0,
  apps_list: 700, app_icon: 25, app_uninstall: 0, app_uninstall_stop_waiting: 20, app_leftovers: 1200, orphan_leftovers: 1600,
  leftovers_remove: 1500, dupes_default_roots: 60, dupes_start: 80, dupes_cancel: 20, dupes_result: 350,
  dupes_thumb: 120, dupes_delete: 1200,
  cache_get_many: 4, app_ready: 0, set_theme_pref: 2, perf_mark: 0, perf_enabled: 0,
};

async function invoke(cmd, args) {
  const h = handlers[cmd];
  if (!h) throw `Unknown command: ${cmd}`;
  await sleep(DELAYS[cmd] ?? rint(60, 400));
  return await h(args ?? {});
}

async function dialogOpen(opts) {
  await sleep(150);
  console.info("[mock] dialog.open", opts);
  return mockState.dialogResult ?? null;
}

export function installMock() {
  if (typeof window === "undefined" || window.__TAURI__) return;
  window.__TAURI__ = {
    core: { invoke },
    event: { listen },
    dialog: { open: dialogOpen },
  };
  window.__PLATTER_MOCK__ = { mockState, mockEmit };
}
